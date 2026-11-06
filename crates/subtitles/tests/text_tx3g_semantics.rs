use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, Time, WorkBudget};
use triergon_subtitles::text::tx3g_config::ConfigLayout;
use triergon_subtitles::text::tx3g_semantics::{
    PaintIntent, PointQuery, SemanticJob, SemanticLimits, SemanticStatus, Semantics, Truth,
};
use triergon_subtitles::text::{Encoding, Format, Input, Timing};

fn source(pool: &Budget, bytes: &[u8], config: Option<&[u8]>) -> Input {
    Input {
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: Bytes::copy_from_slice(pool, bytes).unwrap(),
        codec_id: None,
        codec_private: config.map(|b| Bytes::copy_from_slice(pool, b).unwrap()),
        charset_label: Some(Bytes::copy_from_slice(pool, b"opaque-owner-label").unwrap()),
        addition: None,
        timing: Timing::from_ticks(triergon_subtitles::text::RawClock {
            numerator: 1,
            denominator: 90000,
            pts_ticks: Some(0),
            dts_ticks: None,
            duration_ticks: Some(135000),
        })
        .unwrap(),
        source_id: SourceId(501),
        generation: 7,
        sequence: 11,
        configuration_epoch: 37,
        container_position: None,
    }
}
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = (u32::try_from(body.len() + 8).unwrap())
        .to_be_bytes()
        .to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}
fn sample(text: &[u8], boxes: &[Vec<u8>]) -> Vec<u8> {
    let mut b = (u16::try_from(text.len()).unwrap()).to_be_bytes().to_vec();
    b.extend_from_slice(text);
    for v in boxes {
        b.extend_from_slice(v);
    }
    b
}
fn defaults() -> Vec<u8> {
    let mut b = vec![0; 30];
    b[23] = 1;
    b[25] = 12;
    b[26..30].copy_from_slice(&[255, 255, 255, 255]);
    b.extend_from_slice(&boxed(
        b"ftab",
        &[0, 1, 0, 1, 5, b'S', b'e', b'r', b'i', b'f'],
    ));
    b
}
fn parse(raw: &[u8], config: Option<&[u8]>, q: u64) -> Result<Semantics, Error> {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let input = source(&pool, raw, config);
    let layout = config.map(|_| ConfigLayout::DefaultsRecord);
    let mut job = SemanticJob::new(
        &input,
        layout,
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..1000000 {
        if job.poll(&mut WorkBudget::new(q), &cancel)? == SemanticStatus::Ready {
            let out = job.take_output(&cancel)?;
            assert!(
                out.source()
                    .input
                    .payload
                    .shares_storage_with(&input.payload)
            );
            assert_eq!(out.source().input.payload.as_ref(), raw);
            assert_eq!(out.source().input.configuration_epoch, 37);
            return Ok(out);
        }
    }
    panic!("bounded semantic operation did not terminate")
}
#[test]
fn original_style_highlight_fixture_resolves_requested_style_and_colour_authority() {
    let raw = include_bytes!("text-fixtures/fixtures/input/tx3g-style-highlight-raw.tx3g");
    for q in [1, 7, 4096] {
        let out = parse(raw, Some(&defaults()), q).unwrap();
        assert_eq!(out.character_count(), 3);
        assert_eq!(out.source().modifiers.len(), 3);
        let mut query = PointQuery::new(&out, 1, Some(0)).unwrap();
        let at = loop {
            if let Some(x) = query
                .poll(&mut WorkBudget::new(q), &AtomicBool::new(false))
                .unwrap()
            {
                break x;
            }
        };
        assert_eq!(at.base_style.unwrap().rgba, [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(
            at.paint,
            PaintIntent::StaticBackground([0x11, 0x22, 0x33, 0x44])
        );
        assert_eq!(at.karaoke, Truth::No);
    }
}
#[test]
fn original_duplicate_wrap_fixture_keeps_source_but_strict_semantics_refuses_ambiguity() {
    let raw = include_bytes!("text-fixtures/fixtures/input/tx3g-modifier-order.tx3g");
    assert!(matches!(
        parse(raw, Some(&defaults()), 1),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn karaoke_bounds_use_actual_track_ticks_not_assumed_milliseconds() {
    let mut body = 9000u32.to_be_bytes().to_vec();
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&135000u32.to_be_bytes());
    body.extend_from_slice(&[0, 0, 0, 3]);
    let raw = sample(b"abc", &[boxed(b"krok", &body)]);
    let out = parse(&raw, Some(&defaults()), 1).unwrap();
    assert_eq!(out.karaoke()[0].start_tick, 9000);
    assert_eq!(out.karaoke()[0].end_tick, 135000);
    assert_eq!(out.karaoke()[0].end_time, Some(Time::new(3, 2).unwrap()));
    body[6..10].copy_from_slice(&135001u32.to_be_bytes());
    assert!(
        parse(
            &sample(b"abc", &[boxed(b"krok", &body)]),
            Some(&defaults()),
            1
        )
        .is_err()
    );
}

use std::sync::atomic::Ordering;
use triergon_core::Error;
use triergon_subtitles::text::tx3g_semantics::{
    ScrollDirection, StyleOrigin, ValueOrigin, WrapMode,
};
fn point(
    out: &Semantics,
    character: usize,
    tick: Option<u32>,
    q: u64,
) -> triergon_subtitles::text::tx3g_semantics::PointStyle {
    let mut query = PointQuery::new(out, character, tick).unwrap();
    for _ in 0..1000 {
        if let Some(x) = query
            .poll(&mut WorkBudget::new(q), &AtomicBool::new(false))
            .unwrap()
        {
            return x;
        }
    }
    panic!("bounded point query");
}
fn style(start: u16, end: u16, font: u16, flags: u8, size: u8, rgba: [u8; 4]) -> Vec<u8> {
    let mut b = Vec::new();
    for value in [start, end, font] {
        b.extend_from_slice(&value.to_be_bytes());
    }
    b.extend_from_slice(&[flags, size]);
    b.extend_from_slice(&rgba);
    b
}
fn styles(records: &[Vec<u8>]) -> Vec<u8> {
    let mut b = (u16::try_from(records.len()).unwrap())
        .to_be_bytes()
        .to_vec();
    for r in records {
        b.extend_from_slice(r);
    }
    boxed(b"styl", &b)
}
fn spanbox(kind: &[u8; 4], start: u16, end: u16) -> Vec<u8> {
    let mut b = start.to_be_bytes().to_vec();
    b.extend_from_slice(&end.to_be_bytes());
    boxed(kind, &b)
}
fn karaoke(start: u32, events: &[(u32, u16, u16)]) -> Vec<u8> {
    let mut b = start.to_be_bytes().to_vec();
    b.extend_from_slice(&(u16::try_from(events.len()).unwrap()).to_be_bytes());
    for (end, a, z) in events {
        b.extend_from_slice(&end.to_be_bytes());
        b.extend_from_slice(&a.to_be_bytes());
        b.extend_from_slice(&z.to_be_bytes());
    }
    boxed(b"krok", &b)
}
fn link(start: u16, end: u16, url: &[u8], alt: &[u8]) -> Vec<u8> {
    let mut b = start.to_be_bytes().to_vec();
    b.extend_from_slice(&end.to_be_bytes());
    b.push(u8::try_from(url.len()).unwrap());
    b.extend_from_slice(url);
    b.push(u8::try_from(alt.len()).unwrap());
    b.extend_from_slice(alt);
    boxed(b"href", &b)
}
#[test]
fn sample_styles_override_description_only_on_the_declared_half_open_ranges() {
    let raw = sample(
        b"abcde",
        &[styles(&[style(1, 3, 1, 6, 24, [10, 20, 30, 0])])],
    );
    for q in [1, 7, 4096] {
        let out = parse(&raw, Some(&defaults()), q).unwrap();
        assert_eq!(out.runs().len(), 3);
        assert_eq!(point(&out, 0, Some(0), q).base_style.unwrap().font_size, 12);
        let at = point(&out, 1, Some(0), q);
        assert_eq!(at.style_origin, Some(StyleOrigin::Sample(0)));
        let s = at.base_style.unwrap();
        assert!(s.italic() && s.underline() && !s.bold());
        assert_eq!(s.rgba, [10, 20, 30, 0]);
        assert_eq!(point(&out, 3, Some(0), q).base_style.unwrap().font_size, 12);
        assert_eq!(out.runs()[1].source_bytes, 3..5);
        assert_eq!(out.runs()[1].utf8_bytes, 1..3);
    }
}
#[test]
fn default_style_and_all_rgba_bytes_stay_exact_without_renderer_substitution() {
    let mut cfg = defaults();
    cfg[24] = 0x87;
    cfg[25] = 0;
    cfg[26..30].copy_from_slice(&[0, 17, 255, 0]);
    let out = parse(&sample(b"x", &[]), Some(&cfg), 1).unwrap();
    let s = point(&out, 0, Some(0), 1).base_style.unwrap();
    assert_eq!(s.rgba, [0, 17, 255, 0]);
    assert_eq!(s.font_size, 0);
    assert_eq!(s.uninterpreted_face_bits(), 0x80);
}
#[test]
fn adjacent_style_runs_remain_distinct_source_records() {
    let raw = sample(
        b"abc",
        &[styles(&[
            style(0, 1, 1, 1, 12, [1; 4]),
            style(1, 3, 1, 1, 12, [1; 4]),
        ])],
    );
    let out = parse(&raw, Some(&defaults()), 1).unwrap();
    assert_eq!(out.styles().len(), 2);
    assert_eq!(out.runs().len(), 2);
    assert_ne!(out.runs()[0].style_origin, out.runs()[1].style_origin);
}
#[test]
fn overlapping_and_out_of_order_style_records_are_explicit_errors() {
    for ranges in [[(0, 2), (1, 3)], [(2, 3), (0, 1)], [(0, 3), (0, 0)]] {
        let rs = ranges.map(|(a, z)| style(a, z, 1, 0, 12, [1; 4]));
        assert!(matches!(
            parse(&sample(b"abc", &[styles(&rs)]), Some(&defaults()), 1),
            Err(Error::Invalid(_))
        ));
    }
}
#[test]
fn overlapping_styles_across_separate_boxes_are_not_last_wins() {
    let raw = sample(
        b"abc",
        &[
            styles(&[style(0, 2, 1, 1, 12, [1; 4])]),
            styles(&[style(1, 3, 1, 2, 24, [2; 4])]),
        ],
    );
    assert!(parse(&raw, Some(&defaults()), 1).is_err());
}
#[test]
fn zero_length_style_records_are_preserved_and_apply_to_no_character() {
    let out = parse(
        &sample(b"abc", &[styles(&[style(1, 1, 1, 7, 99, [1; 4])])]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.styles()[0].span.characters, 1..1);
    for c in 0..3 {
        assert_eq!(point(&out, c, Some(0), 1).base_style.unwrap().font_size, 12);
    }
    assert_eq!(out.runs().len(), 1);
}
#[test]
fn unicode_scalar_offsets_are_not_utf8_byte_or_rendered_glyph_offsets() {
    let out = parse(
        &sample(
            "Å水e\u{301}😀".as_bytes(),
            &[styles(&[style(1, 4, 1, 1, 20, [9; 4])])],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.character_count(), 5);
    assert_eq!(out.runs()[1].characters, 1..4);
    assert_eq!(out.runs()[1].utf8_bytes, 2..8);
    assert_eq!(out.runs()[1].source_bytes, 4..10);
}
#[test]
fn utf16_bom_is_excluded_while_surrogate_pairs_are_one_scalar() {
    let out = parse(
        &sample(
            &[0xfe, 0xff, 0xd8, 0x3d, 0xde, 0, 0, 0x41],
            &[spanbox(b"hlit", 0, 1)],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.character_count(), 2);
    assert_eq!(out.runs()[0].source_bytes, 4..8);
    assert_eq!(out.runs()[0].utf8_bytes, 0..4);
    assert_eq!(out.source().text_range, 2..10);
}
#[test]
fn all_line_break_and_nonprinting_scalars_count_in_style_offsets() {
    let text = "a\r\nb\0\u{85}\u{2028}\u{2029}";
    let out = parse(
        &sample(text.as_bytes(), &[styles(&[style(1, 3, 1, 1, 20, [7; 4])])]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.character_count(), 8);
    assert_eq!(
        out.source().decoded_utf8.as_ref().unwrap().as_ref(),
        text.as_bytes()
    );
    assert_eq!(out.runs()[1].characters, 1..3);
}
#[test]
fn highlight_n_plus_one_sentinel_is_retained_as_a_derived_endpoint() {
    let out = parse(
        &sample(b"abc", &[spanbox(b"hlit", 1, 4)]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let range = &out.highlights()[0].span;
    assert_eq!(range.declared_end, 4);
    assert_eq!(range.characters, 1..3);
    assert!(range.end_of_text_sentinel);
}
#[test]
fn style_and_blink_ranges_do_not_adopt_the_highlight_sentinel() {
    for b in [
        styles(&[style(0, 4, 1, 0, 12, [1; 4])]),
        spanbox(b"blnk", 0, 4),
    ] {
        assert!(parse(&sample(b"abc", &[b]), Some(&defaults()), 1).is_err());
    }
}
#[test]
fn inverted_and_beyond_text_ranges_refuse_without_clamping() {
    for (a, z) in [(2, 1), (0, 5), (4, 4)] {
        assert!(
            parse(
                &sample(b"abc", &[spanbox(b"hlit", a, z)]),
                Some(&defaults()),
                1
            )
            .is_err()
        );
    }
}
#[test]
fn static_highlight_without_colour_requests_terminal_inversion() {
    let out = parse(
        &sample(b"abc", &[spanbox(b"hlit", 0, 2)]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(
        point(&out, 1, Some(0), 1).paint,
        PaintIntent::StaticTerminalInvert
    );
}
#[test]
fn highlight_colour_order_does_not_change_semantic_authority() {
    for boxes in [
        [boxed(b"hclr", &[0, 1, 2, 0]), spanbox(b"hlit", 0, 2)],
        [spanbox(b"hlit", 0, 2), boxed(b"hclr", &[0, 1, 2, 0])],
    ] {
        let out = parse(&sample(b"abc", &boxes), Some(&defaults()), 1).unwrap();
        assert_eq!(
            point(&out, 1, Some(0), 1).paint,
            PaintIntent::StaticBackground([0, 1, 2, 0])
        );
    }
}
#[test]
fn hyperlink_requests_terminal_style_over_base_and_static_highlight() {
    let out = parse(
        &sample(
            b"abc",
            &[
                styles(&[style(0, 3, 1, 1, 20, [7; 4])]),
                spanbox(b"hlit", 0, 3),
                link(1, 2, b"relative", b"hint"),
                spanbox(b"blnk", 1, 2),
            ],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let at = point(&out, 1, Some(0), 1);
    assert_eq!(at.paint, PaintIntent::HyperlinkTerminalStyle);
    assert!(at.blink_requested && at.static_highlight);
    assert_eq!(at.base_style.unwrap().rgba, [7; 4]);
    let h = &out.hyperlinks()[0];
    assert_eq!(
        &out.source().input.payload.as_ref()[h.url.clone()],
        b"relative"
    );
    assert_eq!(
        &out.source().input.payload.as_ref()[h.alternate.clone()],
        b"hint"
    );
}
#[test]
fn hyperlink_utf8_url_alt_fields_are_strict_length_delimited() {
    let out = parse(
        &sample(
            b"a",
            &[link(0, 1, "https://x/水".as_bytes(), "Å".as_bytes())],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.hyperlinks().len(), 1);
    for (url, alt) in [
        (&[0xff][..], b"x".as_slice()),
        (b"x".as_slice(), &[0xc0, 0x80][..]),
    ] {
        assert!(parse(&sample(b"a", &[link(0, 1, url, alt)]), Some(&defaults()), 1).is_err());
    }
}
#[test]
fn hyperlink_empty_strings_are_preserved_not_invented_absolute_urls() {
    let out = parse(&sample(b"a", &[link(0, 1, b"", b"")]), Some(&defaults()), 1).unwrap();
    assert!(out.hyperlinks()[0].url.is_empty() && out.hyperlinks()[0].alternate.is_empty());
}
#[test]
fn overlaps_within_highlights_links_and_blinks_are_rejected() {
    for boxes in [
        vec![spanbox(b"hlit", 0, 2), spanbox(b"hlit", 1, 3)],
        vec![link(0, 2, b"a", b""), link(1, 3, b"b", b"")],
        vec![spanbox(b"blnk", 0, 2), spanbox(b"blnk", 1, 3)],
    ] {
        assert!(parse(&sample(b"abc", &boxes), Some(&defaults()), 1).is_err());
    }
}
#[test]
fn karaoke_events_have_half_open_time_intervals_and_zero_range_pauses() {
    let out = parse(
        &sample(
            b"abc",
            &[karaoke(10, &[(20, 0, 1), (30, 1, 1), (40, 1, 3)])],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    for (tick, c, expected) in [
        (9, 0, Truth::No),
        (10, 0, Truth::Yes),
        (19, 0, Truth::Yes),
        (20, 0, Truth::No),
        (25, 1, Truth::No),
        (30, 1, Truth::Yes),
        (40, 1, Truth::No),
    ] {
        assert_eq!(point(&out, c, Some(tick), 1).karaoke, expected);
    }
}
#[test]
fn continuous_karaoke_resolves_prefix_from_source_display_flag() {
    let mut cfg = defaults();
    cfg[..4].copy_from_slice(&0x800u32.to_be_bytes());
    let out = parse(
        &sample(b"abc", &[karaoke(0, &[(10, 1, 2), (20, 2, 3)])]),
        Some(&cfg),
        1,
    )
    .unwrap();
    assert_eq!(point(&out, 0, Some(1), 1).karaoke, Truth::Yes);
    assert_eq!(point(&out, 2, Some(1), 1).karaoke, Truth::No);
}
#[test]
fn unknown_description_keeps_continuous_extent_unresolved() {
    let out = parse(&sample(b"abc", &[karaoke(0, &[(10, 1, 2)])]), None, 1).unwrap();
    assert_eq!(point(&out, 0, Some(1), 1).karaoke, Truth::Unknown);
    assert_eq!(point(&out, 1, Some(1), 1).karaoke, Truth::Yes);
    assert_eq!(
        point(&out, 0, Some(1), 1).paint,
        PaintIntent::UnresolvedDynamicExtent
    );
}
#[test]
fn unknown_query_time_is_not_replaced_with_zero() {
    let out = parse(
        &sample(b"a", &[karaoke(0, &[(10, 0, 1)])]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(point(&out, 0, None, 1).karaoke, Truth::Unknown);
}
#[test]
fn unknown_time_outside_known_noncontinuous_karaoke_keeps_hyperlink_authority() {
    let out = parse(
        &sample(
            b"abcd",
            &[link(0, 1, b"relative", b""), karaoke(0, &[(10, 2, 3)])],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let at = point(&out, 0, None, 1);
    assert_eq!(at.karaoke, Truth::No);
    assert_eq!(at.paint, PaintIntent::HyperlinkTerminalStyle);
}
#[test]
fn unknown_time_outside_known_noncontinuous_karaoke_keeps_static_highlight() {
    let out = parse(
        &sample(
            b"abcd",
            &[spanbox(b"hlit", 0, 1), karaoke(0, &[(10, 2, 3)])],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let at = point(&out, 0, None, 1);
    assert_eq!(at.karaoke, Truth::No);
    assert_eq!(at.paint, PaintIntent::StaticTerminalInvert);
}
#[test]
fn unknown_time_after_all_karaoke_ends_is_never_dynamic_in_any_description_context() {
    for cfg in [None, Some(defaults())] {
        let out = parse(
            &sample(b"abcd", &[karaoke(0, &[(10, 1, 2)])]),
            cfg.as_deref(),
            1,
        )
        .unwrap();
        let at = point(&out, 3, None, 1);
        assert_eq!(at.karaoke, Truth::No);
        assert_eq!(at.paint, PaintIntent::RequestedStyle);
    }
}
#[test]
fn zero_duration_karaoke_events_are_source_records_but_never_active() {
    let out = parse(
        &sample(b"a", &[karaoke(10, &[(10, 0, 1)])]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.karaoke().len(), 1);
    assert_eq!(point(&out, 0, Some(10), 1).karaoke, Truth::No);
}
#[test]
fn unknown_time_with_only_zero_duration_karaoke_has_no_possible_dynamic_extent() {
    let out = parse(
        &sample(b"abc", &[karaoke(0, &[(0, 0, 3)])]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let at = point(&out, 1, None, 1);
    assert_eq!(at.karaoke, Truth::No);
    assert_eq!(at.paint, PaintIntent::RequestedStyle);
}
#[test]
fn continuous_karaoke_empty_character_range_keeps_the_requested_prefix_during_pause() {
    let mut cfg = defaults();
    cfg[..4].copy_from_slice(&0x800u32.to_be_bytes());
    let out = parse(&sample(b"abc", &[karaoke(0, &[(10, 2, 2)])]), Some(&cfg), 1).unwrap();
    let at = point(&out, 0, Some(1), 1);
    assert_eq!(at.karaoke, Truth::Yes);
    assert_eq!(at.paint, PaintIntent::KaraokeForeground(None));
}
#[test]
fn unknown_time_character_membership_is_logarithmic_resumable_and_allocation_free() {
    let events: Vec<_> = (0..1024u16)
        .map(|i| (u32::from(i) + 1, 2 * i + 1, 2 * i + 2))
        .collect();
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let input = source(
        &pool,
        &sample(&vec![b'a'; 2049], &[karaoke(0, &events)]),
        Some(&defaults()),
    );
    let cancel = AtomicBool::new(false);
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    while job.poll(&mut WorkBudget::new(4096), &cancel).unwrap() != SemanticStatus::Ready {}
    let out = job.take_output(&cancel).unwrap();
    for (character, expected) in [(0, Truth::No), (2047, Truth::Unknown), (2048, Truth::No)] {
        let used = pool.used();
        let mut query = PointQuery::new(&out, character, None).unwrap();
        for _ in 0..32 {
            assert_eq!(query.poll(&mut WorkBudget::new(0), &cancel).unwrap(), None);
        }
        let mut result = None;
        for _ in 0..24 {
            if let Some(x) = query.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                result = Some(x);
                break;
            }
        }
        assert_eq!(result.unwrap().karaoke, expected);
        assert_eq!(pool.used(), used);
    }
}
#[test]
fn duplicate_karaoke_even_empty_and_unordered_times_refuse() {
    for boxes in [
        vec![karaoke(0, &[]), karaoke(0, &[])],
        vec![karaoke(20, &[(19, 0, 1)])],
        vec![karaoke(0, &[(20, 0, 1), (10, 1, 2)])],
    ] {
        assert!(parse(&sample(b"abc", &boxes), Some(&defaults()), 1).is_err());
    }
}
#[test]
fn karaoke_character_ranges_must_be_ordered_nonoverlapping() {
    for events in [vec![(10, 0, 2), (20, 1, 3)], vec![(10, 2, 3), (20, 0, 1)]] {
        assert!(
            parse(
                &sample(b"abc", &[karaoke(0, &events)]),
                Some(&defaults()),
                1
            )
            .is_err()
        );
    }
}
#[test]
fn static_dynamic_and_dynamic_link_conflicts_are_rejected() {
    for extra in [spanbox(b"hlit", 1, 3), link(1, 3, b"x", b"")] {
        assert!(
            parse(
                &sample(b"abc", &[karaoke(0, &[(10, 0, 2)]), extra]),
                Some(&defaults()),
                1
            )
            .is_err()
        );
    }
}
#[test]
fn disjoint_dynamic_static_and_link_ranges_remain_legal() {
    let out = parse(
        &sample(
            b"abcd",
            &[
                karaoke(0, &[(10, 0, 1)]),
                spanbox(b"hlit", 1, 2),
                link(2, 3, b"x", b""),
                spanbox(b"blnk", 0, 4),
            ],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.runs().len(), 4);
}
#[test]
fn continuous_prefix_conflict_is_detected_even_outside_declared_start() {
    let mut cfg = defaults();
    cfg[..4].copy_from_slice(&0x800u32.to_be_bytes());
    assert!(
        parse(
            &sample(
                b"abcd",
                &[karaoke(0, &[(10, 2, 3)]), spanbox(b"hlit", 0, 1)]
            ),
            Some(&cfg),
            1
        )
        .is_err()
    );
}
#[test]
fn missing_description_does_not_fake_complete_combination_validation() {
    let out = parse(
        &sample(
            b"abcd",
            &[karaoke(0, &[(10, 2, 3)]), spanbox(b"hlit", 0, 1)],
        ),
        None,
        1,
    )
    .unwrap();
    assert!(!out.validation_context().conditional_combinations_checked);
}
#[test]
fn dynamic_colour_is_foreground_while_static_colour_is_background() {
    let out = parse(
        &sample(
            b"ab",
            &[
                karaoke(0, &[(10, 0, 1)]),
                spanbox(b"hlit", 1, 2),
                boxed(b"hclr", &[1, 2, 3, 4]),
            ],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(
        point(&out, 0, Some(1), 1).paint,
        PaintIntent::KaraokeForeground(Some([1, 2, 3, 4]))
    );
    assert_eq!(
        point(&out, 1, Some(1), 1).paint,
        PaintIntent::StaticBackground([1, 2, 3, 4])
    );
}
#[test]
fn scroll_delay_uses_track_ticks_and_only_specified_zero_default() {
    let out = parse(
        &sample(b"x", &[boxed(b"dlay", &45000u32.to_be_bytes())]),
        Some(&defaults()),
        1,
    )
    .unwrap();
    assert_eq!(out.globals().scroll_delay_ticks().value, 45000);
    assert_eq!(
        out.globals().scroll_delay_ticks().origin,
        ValueOrigin::Sample
    );
    let out = parse(&sample(b"x", &[]), None, 1).unwrap();
    assert_eq!(out.globals().scroll_delay_ticks().value, 0);
    assert_eq!(
        out.globals().scroll_delay_ticks().origin,
        ValueOrigin::SpecifiedAbsentDefault
    );
}
#[test]
fn sample_textbox_overrides_description_without_clamping_signed_geometry() {
    let b = [0xff, 0xfc, 0xff, 0xf0, 0, 32, 0, 64];
    let out = parse(&sample(b"x", &[boxed(b"tbox", &b)]), Some(&defaults()), 1).unwrap();
    assert_eq!(
        out.globals().resolved_text_box.unwrap().value,
        [-4, -16, 32, 64]
    );
    assert_eq!(
        out.globals().resolved_text_box.unwrap().origin,
        ValueOrigin::Sample
    );
}
#[test]
fn sample_disparity_overrides_description_and_retains_sixteenth_pixels() {
    let mut cfg = defaults();
    cfg.extend_from_slice(&boxed(b"disp", &17i16.to_be_bytes()));
    let out = parse(
        &sample(b"x", &[boxed(b"disp", &(-31i16).to_be_bytes())]),
        Some(&cfg),
        1,
    )
    .unwrap();
    assert_eq!(out.globals().description_disparity, Some(17));
    assert_eq!(out.globals().resolved_disparity.unwrap().value, -31);
    assert_eq!(out.globals().disparity_pixels(), Some((1, -1)));
}
#[test]
fn missing_description_does_not_invent_disparity_zero() {
    let out = parse(&sample(b"x", &[]), None, 1).unwrap();
    assert!(out.globals().resolved_disparity.is_none());
    let out = parse(&sample(b"x", &[]), Some(&defaults()), 1).unwrap();
    assert_eq!(
        out.globals().resolved_disparity.unwrap().origin,
        ValueOrigin::SpecifiedAbsentDefault
    );
}
#[test]
fn disparity_minimum_is_widened_before_negation() {
    let out = parse(
        &sample(b"x", &[boxed(b"disp", &i16::MIN.to_be_bytes())]),
        None,
        1,
    )
    .unwrap();
    assert_eq!(out.globals().disparity_pixels(), Some((2048, -2048)));
}
#[test]
fn all_known_display_flags_and_directions_are_decoded_without_dropping_unknown_bits() {
    for direction in 0..4 {
        let mut cfg = defaults();
        let flags = 0x60860u32 | (direction << 7) | 0x80000000;
        cfg[..4].copy_from_slice(&flags.to_be_bytes());
        let out = parse(&sample(b"x", &[]), Some(&cfg), 1).unwrap();
        let f = out.globals().display.unwrap();
        assert!(f.scroll_in && f.scroll_out && f.continuous_karaoke && f.vertical && f.fill_region);
        assert_eq!(f.unknown_bits, 0x80000000);
        assert_eq!(
            f.scroll_direction,
            [
                ScrollDirection::Up,
                ScrollDirection::Left,
                ScrollDirection::Down,
                ScrollDirection::Right
            ][direction as usize]
        );
    }
}
#[test]
fn wrap_known_modes_and_reserved_modes_have_distinct_outcomes() {
    for (v, mode) in [(0, WrapMode::NoWrap), (1, WrapMode::Soft)] {
        assert_eq!(
            parse(&sample(b"x", &[boxed(b"twrp", &[v])]), None, 1)
                .unwrap()
                .globals()
                .wrap
                .as_ref()
                .unwrap()
                .value,
            mode
        );
    }
    for v in [2, 127, 255] {
        assert!(matches!(
            parse(&sample(b"x", &[boxed(b"twrp", &[v])]), None, 1),
            Err(Error::Unsupported(_))
        ));
    }
}
#[test]
fn duplicate_global_modifiers_never_silently_choose_last() {
    for (kind, body) in [
        (b"hclr", vec![1; 4]),
        (b"dlay", vec![0; 4]),
        (b"tbox", vec![0; 8]),
        (b"disp", vec![0; 2]),
        (b"twrp", vec![0]),
    ] {
        let b = boxed(kind, &body);
        assert!(parse(&sample(b"x", &[b.clone(), b]), Some(&defaults()), 1).is_err());
    }
}
#[test]
fn unknown_modifiers_remain_full_source_ranges_without_semantic_effect() {
    let raw = include_bytes!("text-fixtures/fixtures/input/tx3g-unknown-modifier.tx3g");
    let out = parse(raw, Some(&defaults()), 1).unwrap();
    assert_eq!(out.source().modifiers.len(), 1);
    assert_eq!(out.source().modifiers.as_slice()[0].kind, *b"zZ09");
    assert_eq!(out.runs().len(), 1);
}
#[test]
fn extended_and_to_end_known_boxes_reuse_existing_physical_box_rules() {
    let mut extended = 1u32.to_be_bytes().to_vec();
    extended.extend_from_slice(b"twrp");
    extended.extend_from_slice(&17u64.to_be_bytes());
    extended.push(1);
    let mut toend = 0u32.to_be_bytes().to_vec();
    toend.extend_from_slice(b"disp");
    toend.extend_from_slice(&17i16.to_be_bytes());
    let out = parse(&sample(b"x", &[extended, toend]), None, 1).unwrap();
    assert_eq!(out.globals().wrap.as_ref().unwrap().value, WrapMode::Soft);
    assert_eq!(out.globals().resolved_disparity.unwrap().value, 17);
}
#[test]
fn known_box_trailing_bytes_and_short_bodies_are_errors() {
    for (kind, length) in [
        (b"hlit", 4),
        (b"hclr", 4),
        (b"dlay", 4),
        (b"tbox", 8),
        (b"blnk", 4),
        (b"disp", 2),
        (b"twrp", 1),
    ] {
        for len in [length - 1, length + 1] {
            assert!(parse(&sample(b"abc", &[boxed(kind, &vec![0; len])]), None, 1).is_err());
        }
    }
}
#[test]
fn malformed_advertised_record_counts_do_not_publish_partial_semantics() {
    for b in [
        boxed(b"styl", &[0, 1]),
        boxed(b"styl", &[0, 0, 0]),
        boxed(b"krok", &[0, 0, 0, 0, 0, 1]),
    ] {
        assert!(parse(&sample(b"x", &[b]), None, 1).is_err());
    }
}
#[test]
fn font_names_are_decoded_from_source_utf8_or_utf16_bom_without_selecting_renderer_fallback() {
    let mut cfg = vec![0; 30];
    cfg[23] = 7;
    cfg[25] = 12;
    let mut fonts = vec![0, 2, 0, 7, 6, 0xfe, 0xff, 0, 0x41, 0, 0x42];
    fonts.extend_from_slice(&[0, 9, 9]);
    fonts.extend_from_slice(b"Noto,Mono");
    cfg.extend_from_slice(&boxed(b"ftab", &fonts));
    let out = parse(&sample(b"x", &[]), Some(&cfg), 1).unwrap();
    assert_eq!(out.font(7).unwrap().name_utf8.as_ref(), b"AB");
    assert!(out.font(7).unwrap().utf16_bom);
    assert_eq!(out.font(9).unwrap().name_utf8.as_ref(), b"Noto,Mono");
}
#[test]
fn duplicate_and_missing_font_ids_are_explicit_semantic_errors() {
    let mut cfg = vec![0; 30];
    cfg[23] = 1;
    cfg.extend_from_slice(&boxed(b"ftab", &[0, 2, 0, 1, 1, b'A', 0, 1, 1, b'B']));
    assert!(parse(&sample(b"x", &[]), Some(&cfg), 1).is_err());
    assert!(
        parse(
            &sample(b"x", &[styles(&[style(0, 1, 2, 0, 12, [1; 4])])]),
            Some(&defaults()),
            1
        )
        .is_err()
    );
}
#[test]
fn missing_description_keeps_unbound_font_id_and_base_style_unknown() {
    let out = parse(
        &sample(b"ab", &[styles(&[style(1, 2, 99, 1, 12, [1; 4])])]),
        None,
        1,
    )
    .unwrap();
    assert!(point(&out, 0, Some(0), 1).base_style.is_none());
    assert_eq!(point(&out, 1, Some(0), 1).base_style.unwrap().font_id, 99);
    assert!(point(&out, 1, Some(0), 1).font_index.is_none());
}
#[test]
fn explicit_clock_absence_keeps_relative_times_unknown_even_with_raw_input_clock() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = sample(b"a", &[karaoke(0, &[(90000, 0, 1)])]);
    let input = source(&pool, &raw, Some(&defaults()));
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        None,
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    while job
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != SemanticStatus::Ready
    {}
    let out = job.take_output(&AtomicBool::new(false)).unwrap();
    assert!(out.karaoke()[0].end_time.is_none());
    assert!(!out.validation_context().duration_bounds_checked);
    assert_eq!(out.karaoke()[0].end_tick, 90000);
}
#[test]
fn missing_duration_does_not_invent_a_time_bound_or_next_packet_end() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = sample(b"a", &[karaoke(0, &[(u32::MAX, 0, 1)])]);
    let mut input = source(&pool, &raw, Some(&defaults()));
    input.timing = Timing::unknown();
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    while job
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != SemanticStatus::Ready
    {}
    let out = job.take_output(&AtomicBool::new(false)).unwrap();
    assert!(!out.validation_context().duration_bounds_checked);
    assert!(out.karaoke()[0].end_time.is_some());
    assert!(out.source().end.is_none());
}
#[test]
fn zero_clock_and_missing_explicit_description_source_are_constructor_errors() {
    let pool = Budget::new(1 << 20).unwrap();
    let input = source(&pool, &sample(b"x", &[]), None);
    assert!(matches!(
        SemanticJob::new(&input, None, Some(0), &pool, SemanticLimits::default(), 7),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        SemanticJob::new(
            &input,
            Some(ConfigLayout::DefaultsRecord),
            Some(90000),
            &pool,
            SemanticLimits::default(),
            7
        ),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn stale_generation_is_rejected_while_original_caller_owners_remain() {
    let pool = Budget::new(8192).unwrap();
    let raw = sample(b"x", &[]);
    let input = source(&pool, &raw, None);
    let baseline = pool.used();
    assert!(matches!(
        SemanticJob::new(
            &input,
            None,
            Some(90000),
            &pool,
            SemanticLimits::default(),
            8
        ),
        Err(Error::StaleGeneration { .. })
    ));
    assert_eq!(input.payload.as_ref(), raw);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn foreign_payload_configuration_and_label_owners_never_enter_the_job() {
    for role in 0..3 {
        let pool = Budget::new(16384).unwrap();
        let foreign = Budget::new(16384).unwrap();
        let mut input = source(&pool, &sample(b"x", &[]), Some(&defaults()));
        match role {
            0 => input.payload = Bytes::copy_from_slice(&foreign, &sample(b"x", &[])).unwrap(),
            1 => input.codec_private = Some(Bytes::copy_from_slice(&foreign, &defaults()).unwrap()),
            _ => input.charset_label = Some(Bytes::copy_from_slice(&foreign, b"raw").unwrap()),
        }
        let baseline = pool.used();
        assert!(matches!(
            SemanticJob::new(
                &input,
                Some(ConfigLayout::DefaultsRecord),
                Some(90000),
                &pool,
                SemanticLimits::default(),
                7
            ),
            Err(Error::ForeignPool)
        ));
        assert_eq!(pool.used(), baseline);
    }
}
#[test]
fn zero_work_semantics_performs_no_progress_or_allocation() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = sample(b"abc", &[spanbox(b"hlit", 0, 2)]);
    let input = source(&pool, &raw, Some(&defaults()));
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    let baseline = pool.used();
    for _ in 0..10 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            SemanticStatus::Pending
        );
        assert_eq!(pool.used(), baseline);
    }
    while job
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != SemanticStatus::Ready
    {}
    assert_eq!(
        job.take_output(&AtomicBool::new(false))
            .unwrap()
            .character_count(),
        3
    );
}
#[test]
fn query_zero_work_and_cancel_preserve_search_progress() {
    let out = parse(
        &sample(
            b"abc",
            &[
                styles(&[style(1, 2, 1, 1, 24, [1; 4])]),
                karaoke(0, &[(10, 0, 1)]),
            ],
        ),
        Some(&defaults()),
        1,
    )
    .unwrap();
    let mut q = PointQuery::new(&out, 1, Some(1)).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..5 {
        assert!(q.poll(&mut WorkBudget::new(0), &c).unwrap().is_none());
    }
    assert!(q.poll(&mut WorkBudget::new(1), &c).unwrap().is_none());
    c.store(true, Ordering::Relaxed);
    assert_eq!(q.poll(&mut WorkBudget::new(1), &c), Err(Error::Cancelled));
    c.store(false, Ordering::Relaxed);
    let actual = loop {
        if let Some(v) = q.poll(&mut WorkBudget::new(1), &c).unwrap() {
            break v;
        }
    };
    assert_eq!(actual, point(&out, 1, Some(1), 4096));
}
#[test]
fn cancellation_at_every_initial_single_unit_boundary_is_resumable() {
    let raw = sample(
        b"abc",
        &[
            styles(&[style(0, 3, 1, 1, 20, [9; 4])]),
            spanbox(b"hlit", 1, 3),
            boxed(b"hclr", &[1, 2, 3, 4]),
        ],
    );
    for pause in 0..256 {
        let pool = Budget::new(1 << 20).unwrap();
        let input = source(&pool, &raw, Some(&defaults()));
        let c = AtomicBool::new(false);
        let mut job = SemanticJob::new(
            &input,
            Some(ConfigLayout::DefaultsRecord),
            Some(90000),
            &pool,
            SemanticLimits::default(),
            7,
        )
        .unwrap();
        for _ in 0..pause {
            if job.poll(&mut WorkBudget::new(1), &c).unwrap() == SemanticStatus::Ready {
                break;
            }
        }
        let baseline = pool.used();
        c.store(true, Ordering::Relaxed);
        assert_eq!(job.poll(&mut WorkBudget::new(1), &c), Err(Error::Cancelled));
        assert_eq!(pool.used(), baseline);
        c.store(false, Ordering::Relaxed);
        while job.poll(&mut WorkBudget::new(1), &c).unwrap() != SemanticStatus::Ready {}
        let out = job.take_output(&c).unwrap();
        assert_eq!(out.character_count(), 3);
        assert_eq!(out.styles().len(), 1);
        assert_eq!(out.highlights().len(), 1);
        assert_eq!(
            point(&out, 1, Some(0), 1).paint,
            PaintIntent::StaticBackground([1, 2, 3, 4])
        );
    }
}
#[test]
fn retained_source_description_font_and_decoded_owners_release_only_after_output() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = sample(&[0xfe, 0xff, 0, 65], &[]);
    let input = source(&pool, &raw, Some(&defaults()));
    let original = input.payload.clone();
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != SemanticStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert!(out.source().input.payload.shares_storage_with(&original));
    drop(job);
    drop(input);
    drop(original);
    assert!(pool.used() > Budget::control_storage_bytes());
    assert_eq!(out.source().decoded_utf8.as_ref().unwrap().as_ref(), b"A");
    assert_eq!(out.font(1).unwrap().name_utf8.as_ref(), b"Serif");
    drop(out);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn record_limit_failure_is_sticky_frees_construction_and_publishes_nothing() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = sample(
        b"abc",
        &[styles(&[
            style(0, 1, 1, 1, 12, [1; 4]),
            style(1, 2, 1, 2, 12, [2; 4]),
        ])],
    );
    let input = source(&pool, &raw, Some(&defaults()));
    let baseline = pool.used();
    let limits = SemanticLimits {
        maximum_records: 1,
        ..SemanticLimits::default()
    };
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        limits,
        7,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    let error = loop {
        match job.poll(&mut WorkBudget::new(1), &c) {
            Ok(SemanticStatus::Pending) => {}
            Ok(_) => panic!("limited operation published"),
            Err(e) => break e,
        }
    };
    assert!(matches!(error, Error::Limit { .. }));
    assert_eq!(job.poll(&mut WorkBudget::new(4096), &c), Err(error));
    assert!(job.take_output(&c).is_err());
    assert_eq!(pool.used(), baseline);
}
#[test]
fn derived_run_limit_preserves_real_boundaries_and_never_coalesces_to_pass() {
    let pool = Budget::new(1 << 20).unwrap();
    let input = source(
        &pool,
        &sample(b"abc", &[styles(&[style(1, 2, 1, 1, 12, [1; 4])])]),
        Some(&defaults()),
    );
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits {
            maximum_runs: 1,
            ..SemanticLimits::default()
        },
        7,
    )
    .unwrap();
    let error = loop {
        match job.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(SemanticStatus::Pending) => {}
            Ok(_) => panic!("run limit was hidden"),
            Err(e) => break e,
        }
    };
    assert!(matches!(error, Error::Limit { .. }));
}
#[test]
fn pool_pressure_and_repeated_zero_work_retry_do_not_duplicate_modifiers() {
    let pool = Budget::new(16384).unwrap();
    let input = source(
        &pool,
        &sample(b"abc", &[spanbox(b"hlit", 1, 3)]),
        Some(&defaults()),
    );
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    let pressure = pool.reserve(pool.available()).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..200 {
        match job.poll(&mut WorkBudget::new(1), &c) {
            Ok(SemanticStatus::Pending | SemanticStatus::NeedOutput)
            | Err(Error::Backpressure { .. }) => {}
            other => panic!("unexpected pressure result {other:?}"),
        }
    }
    let used = pool.used();
    for _ in 0..5 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &c).unwrap(),
            SemanticStatus::Pending
        );
        assert_eq!(pool.used(), used);
    }
    drop(pressure);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != SemanticStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert_eq!(out.highlights().len(), 1);
    assert_eq!(out.source().modifiers.len(), 1);
    assert_eq!(out.runs().len(), 2);
}
#[test]
fn malformed_late_modifier_never_publishes_partial_semantic_output() {
    let pool = Budget::new(16384).unwrap();
    let raw = sample(
        b"abc",
        &[
            spanbox(b"hlit", 0, 1),
            boxed(b"href", &[0, 1, 0, 2, 3, b'x']),
        ],
    );
    let input = source(&pool, &raw, Some(&defaults()));
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    loop {
        match job.poll(&mut WorkBudget::new(1), &c) {
            Ok(SemanticStatus::Pending) => assert!(job.take_output(&c).is_err()),
            Ok(_) => panic!("partial semantic result published"),
            Err(_) => break,
        }
    }
    assert_eq!(input.payload.as_ref(), raw);
}
#[test]
fn empty_text_has_no_runs_and_no_queryable_character() {
    let out = parse(&sample(b"", &[]), Some(&defaults()), 1).unwrap();
    assert!(out.runs().is_empty());
    assert!(PointQuery::new(&out, 0, Some(0)).is_err());
}
#[test]
fn full_u16_byte_count_is_not_reduced_to_recommended_2048() {
    let raw = sample(&vec![b'x'; 65535], &[]);
    let out = parse(&raw, None, 1).unwrap();
    assert_eq!(out.character_count(), 65535);
    assert_eq!(out.runs()[0].source_bytes, 2..65537);
    assert!(PointQuery::new(&out, 65535, Some(0)).is_err());
    assert!(point(&out, 65534, Some(0), 1).base_style.is_none());
}
#[test]
fn zero_font_id_and_sparse_high_ids_do_not_use_truthiness_as_knownness() {
    let mut cfg = vec![0; 30];
    cfg[25] = 12;
    cfg.extend_from_slice(&boxed(b"ftab", &[0, 2, 0, 0, 1, b'A', 255, 255, 1, b'B']));
    let out = parse(
        &sample(b"ab", &[styles(&[style(1, 2, 65535, 0, 12, [1; 4])])]),
        Some(&cfg),
        1,
    )
    .unwrap();
    assert_eq!(out.font(0).unwrap().name_utf8.as_ref(), b"A");
    assert_eq!(out.font(65535).unwrap().name_utf8.as_ref(), b"B");
    assert_eq!(point(&out, 0, Some(0), 1).font_index, Some(0));
}
#[test]
fn unknown_configuration_layout_does_not_infer_default_style_or_disparity() {
    let pool = Budget::new(16384).unwrap();
    let input = source(&pool, &sample(b"x", &[]), Some(b"opaque"));
    let mut job = SemanticJob::new(
        &input,
        None,
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    while job
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != SemanticStatus::Ready
    {}
    let out = job.take_output(&AtomicBool::new(false)).unwrap();
    assert!(!out.validation_context().description_parsed);
    assert!(out.globals().resolved_disparity.is_none());
    assert!(out.runs()[0].style.is_none());
    assert_eq!(
        out.source().input.codec_private.as_ref().unwrap().as_ref(),
        b"opaque"
    );
}
#[test]
fn output_is_taken_once_and_old_owners_survive_job_drop() {
    let pool = Budget::new(16384).unwrap();
    let input = source(&pool, &sample(b"x", &[]), Some(&defaults()));
    let mut job = SemanticJob::new(
        &input,
        Some(ConfigLayout::DefaultsRecord),
        Some(90000),
        &pool,
        SemanticLimits::default(),
        7,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != SemanticStatus::Ready {}
    let out = job.take_output(&c).unwrap();
    assert!(job.take_output(&c).is_err());
    drop(job);
    assert_eq!(out.source().decoded_utf8.as_ref().unwrap().as_ref(), b"x");
}

#[test]
fn empty_karaoke_header_start_obeys_known_sample_duration() {
    let raw = sample(b"a", &[karaoke(135001, &[])]);
    assert!(matches!(
        parse(&raw, Some(&defaults()), 1),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn zero_duration_tail_never_extends_a_continuous_conflict_prefix() {
    let mut cfg = defaults();
    cfg[..4].copy_from_slice(&0x800u32.to_be_bytes());
    let raw = sample(
        b"abcdefgh",
        &[
            spanbox(b"hlit", 4, 5),
            karaoke(0, &[(10, 1, 2), (10, 6, 7)]),
        ],
    );
    for q in [1, 7, 4096] {
        let out = parse(&raw, Some(&cfg), q).unwrap();
        assert_eq!(out.karaoke().len(), 2);
        assert_eq!(out.karaoke()[1].span.characters, 6..7);
        assert_eq!(out.karaoke()[1].start_tick, 10);
        assert_eq!(out.karaoke()[1].end_tick, 10);
        for tick in [None, Some(0), Some(5), Some(9), Some(10), Some(11)] {
            let at = point(&out, 4, tick, q);
            assert_eq!(at.karaoke, Truth::No);
            assert_eq!(at.paint, PaintIntent::StaticTerminalInvert);
        }
    }
}
#[test]
fn zero_duration_only_overlap_does_not_conflict_with_static_highlight() {
    let raw = sample(
        b"abcdefgh",
        &[spanbox(b"hlit", 4, 5), karaoke(10, &[(10, 4, 5)])],
    );
    for q in [1, 7, 4096] {
        let out = parse(&raw, Some(&defaults()), q).unwrap();
        assert_eq!(out.karaoke()[0].span.characters, 4..5);
        for tick in [None, Some(0), Some(9), Some(10), Some(11)] {
            let at = point(&out, 4, tick, q);
            assert_eq!(at.karaoke, Truth::No);
            assert_eq!(at.paint, PaintIntent::StaticTerminalInvert);
        }
    }
}
#[test]
fn zero_duration_only_overlap_does_not_conflict_with_hyperlink() {
    let raw = sample(
        b"abcdefgh",
        &[
            link(4, 5, b"relative", b"unchanged"),
            karaoke(10, &[(10, 4, 5)]),
        ],
    );
    for q in [1, 7, 4096] {
        let out = parse(&raw, Some(&defaults()), q).unwrap();
        assert_eq!(out.hyperlinks().len(), 1);
        assert_eq!(out.karaoke().len(), 1);
        for tick in [None, Some(9), Some(10), Some(11)] {
            let at = point(&out, 4, tick, q);
            assert_eq!(at.karaoke, Truth::No);
            assert_eq!(at.paint, PaintIntent::HyperlinkTerminalStyle);
        }
    }
}
#[test]
fn zero_duration_only_events_need_no_invented_continuous_context_for_conflicts() {
    let raw = sample(
        b"abcdefgh",
        &[
            spanbox(b"hlit", 4, 5),
            link(4, 5, b"relative", b""),
            karaoke(10, &[(10, 4, 5)]),
        ],
    );
    let out = parse(&raw, None, 1).unwrap();
    assert!(out.validation_context().conditional_combinations_checked);
    let at = point(&out, 4, None, 1);
    assert_eq!(at.base_style, None);
    assert_eq!(at.karaoke, Truth::No);
    assert_eq!(at.paint, PaintIntent::HyperlinkTerminalStyle);
}
#[test]
fn zero_duration_applicability_does_not_relax_source_record_order_or_ranges() {
    for events in [vec![(10, 4, 5), (10, 1, 2)], vec![(10, 4, 3)]] {
        assert!(matches!(
            parse(
                &sample(b"abcdefgh", &[karaoke(10, &events)]),
                Some(&defaults()),
                1
            ),
            Err(Error::Invalid(_))
        ));
    }
}
#[test]
fn positive_event_after_zero_duration_still_conflicts_with_static_or_link() {
    for effect in [spanbox(b"hlit", 4, 5), link(4, 5, b"relative", b"")] {
        let raw = sample(b"abcdefgh", &[effect, karaoke(0, &[(0, 1, 2), (10, 4, 5)])]);
        assert!(matches!(
            parse(&raw, Some(&defaults()), 1),
            Err(Error::Invalid(_))
        ));
    }
}
