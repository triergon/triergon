#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, ColourProjection, ColourSource, DecodeStatus, Decoder, Depth,
    DisplayEvent, DisplayLimits, PacketIdentity, PacketTiming, ReceiverView, SourceClock,
    SourceConfiguration,
};

struct Case {
    id: &'static str,
    depth: u8,
    payload: &'static [u8],
    indices: &'static [u8],
    native_rgba: &'static [u8],
    native_words: &'static [u32],
    explicit: &'static [(u8, [u8; 4])],
    source_overrides: &'static [(u8, [u8; 4])],
    canvas: [u32; 2],
    rect: [u32; 4],
    pts_microseconds: i64,
}

#[path = "native-defaults-support/cases.rs"]
mod cases;

fn depth(value: u8) -> Depth {
    match value {
        2 => Depth::Two,
        4 => Depth::Four,
        8 => Depth::Eight,
        _ => panic!("authored nominal depth"),
    }
}

// Independently tabulated EN300743 Tables36-38 percentages, projected under the
// existing named SourceNearest8 convention. No production colour routine is used
// to obtain these expected components. Native observations never alter this art.
fn source_default(bits: u8, index: u8) -> [u8; 4] {
    if index == 0 {
        return [0, 0, 0, 0];
    }
    match bits {
        2 => match index {
            1 => [255, 255, 255, 255],
            2 => [0, 0, 0, 255],
            3 => [128, 128, 128, 255],
            _ => panic!("source art index"),
        },
        4 => {
            let enabled = if index < 8 { 255 } else { 128 };
            [
                if index & 1 == 0 { 0 } else { enabled },
                if index & 2 == 0 { 0 } else { enabled },
                if index & 4 == 0 { 0 } else { enabled },
                255,
            ]
        }
        8 if index < 8 => [
            if index & 1 == 0 { 0 } else { 255 },
            if index & 2 == 0 { 0 } else { 255 },
            if index & 4 == 0 { 0 } else { 255 },
            64,
        ],
        8 => {
            let (components, alpha) = match index & 0x88 {
                0 => ([0, 85, 170, 255], 255),
                8 => ([0, 85, 170, 255], 128),
                128 => ([128, 170, 212, 255], 255),
                _ => ([0, 43, 85, 128], 255),
            };
            let pair = |low: u8, high: u8| {
                usize::from(((index >> low) & 1) | (((index >> high) & 1) << 1))
            };
            [
                components[pair(0, 4)],
                components[pair(1, 5)],
                components[pair(2, 6)],
                alpha,
            ]
        }
        _ => panic!("source art depth"),
    }
}

fn source_palette(case: &Case) -> Vec<u8> {
    let mut result = Vec::new();
    for index in 0..(1_u16 << case.depth) {
        let index = u8::try_from(index).unwrap();
        let rgba = case
            .source_overrides
            .iter()
            .find(|(i, _)| *i == index)
            .map_or_else(|| source_default(case.depth, index), |(_, rgba)| *rgba);
        result.extend_from_slice(&rgba);
    }
    result
}

fn decode(pool: &Budget, case: &Case, view: ReceiverView, quantum: u64) -> Vec<DisplayEvent> {
    let cancel = AtomicBool::new(false);
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x10]).unwrap();
    let configuration = SourceConfiguration::from_matroska_private(
        pool,
        &private,
        SourceClock::new(1, 90000).unwrap(),
    )
    .unwrap();
    let mut decoder = Decoder::new_configured_with_receiver_view(
        pool,
        &configuration,
        19,
        DisplayLimits::default(),
        view,
        &cancel,
    )
    .unwrap();
    let payload = Bytes::copy_from_slice(pool, case.payload).unwrap();
    let packet = configuration
        .packet(
            pool,
            &payload,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: 90000,
                    origin: ClockOrigin::Source,
                }),
                dts: None,
                duration: None,
            },
            PacketIdentity {
                generation: 19,
                sequence: 0,
                container_position: None,
            },
        )
        .unwrap();
    decoder.provide(&packet).unwrap();
    let mut events = Vec::new();
    let mut ended = false;
    for _ in 0..1_000_000 {
        let progress = decoder
            .step(&mut WorkBudget::new(quantum), &cancel)
            .unwrap();
        match progress.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => events.push(progress.event.unwrap()),
            DecodeStatus::NeedPacket => {
                assert!(!ended);
                decoder.finish_input().unwrap();
                ended = true;
            }
            DecodeStatus::NeedOutput => panic!("bounded default palette exhausted4MiB"),
            DecodeStatus::Finished => return events,
        }
    }
    panic!("bounded default palette poll ceiling")
}

fn source_facts(event: &DisplayEvent, case: &Case) {
    assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
    assert_eq!(event.timeout_seconds, 2);
    assert_eq!(event.expiry, Some(Time::new(3, 1).unwrap()));
    assert_eq!(
        [event.definition.size.width, event.definition.size.height],
        case.canvas
    );
    assert_eq!(event.source_packets().len(), 1);
    assert_eq!(event.source_packets()[0].payload.as_ref(), case.payload);
    assert_eq!(event.regions().len(), 1);
    let region = &event.regions()[0];
    assert_eq!(
        [
            region.position.x,
            region.position.y,
            region.size.width,
            region.size.height
        ],
        case.rect
    );
    assert_eq!(
        region.plane.geometry.stride,
        usize::try_from(case.rect[2]).unwrap()
    );
    assert_eq!(region.plane.pixels.as_ref(), case.indices);
    assert_eq!(region.palette.rgba.as_ref(), source_palette(case));
    assert_eq!(region.palette.entries().len(), 1_usize << case.depth);
    for (index, entry) in region.palette.entries().iter().enumerate() {
        let index = u8::try_from(index).unwrap();
        if let Some((_, [y, cr, cb, transparency])) =
            case.explicit.iter().find(|(i, _)| *i == index)
        {
            assert!(entry.original_segment.is_some());
            assert_eq!(
                entry.source,
                ColourSource::YCrCbT {
                    y: *y,
                    cr: *cr,
                    cb: *cb,
                    transparency: *transparency,
                    full_resolution: true,
                }
            );
        } else {
            assert!(entry.original_segment.is_none());
            assert_eq!(
                entry.source,
                ColourSource::Default {
                    depth: depth(case.depth),
                    index
                }
            );
        }
    }
}

#[test]
fn all276_normative_default_entries_keep_source_percentages_and_opacity() {
    for bits in [2, 4, 8] {
        for index in 0..(1_u16 << bits) {
            let index = u8::try_from(index).unwrap();
            let raw = ColourSource::Default {
                depth: depth(bits),
                index,
            };
            let expected = source_default(bits, index);
            assert_eq!(raw.rgba_source_nearest().unwrap(), expected);
            assert_eq!(
                raw.rgba(ColourProjection::SourceNearest8).unwrap(),
                expected
            );
            let fraction = if index == 0 {
                (0, 1)
            } else if bits == 8 && index < 8 {
                (1, 4)
            } else if bits == 8 && index & 0x88 == 8 {
                (1, 2)
            } else {
                (1, 1)
            };
            assert_eq!(raw.opacity().unwrap(), fraction);
            let unrepaired = raw.rgba(ColourProjection::StudioUnrepaired8).unwrap();
            assert_eq!(&unrepaired[..3], &expected[..3]);
            assert_eq!(u16::from(unrepaired[3]), fraction.0 * 255 / fraction.1);
        }
    }
    for (depth, index) in [(Depth::Two, 4), (Depth::Four, 16)] {
        assert_eq!(
            ColourSource::Default { depth, index }.rgba_source_nearest(),
            Err(Error::Invalid("DVB default palette index"))
        );
    }
}

#[test]
fn whole_source_no_cds_and_partial_cds_keep_every_unredefined_entry() {
    for case in &cases::CASES {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let events = decode(&pool, case, ReceiverView::Source, quantum);
            assert_eq!(events.len(), 1);
            source_facts(&events[0], case);
            assert!(events[0].regions()[0].palette.native_d32.is_none());
            drop(events);
            assert_eq!(pool.used(), baseline);
        }
    }
}

fn native_case(at: usize) {
    let case = &cases::CASES[at];
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let events = decode(&pool, case, ReceiverView::NativeD32, quantum);
        assert_eq!(events.len(), 1);
        source_facts(&events[0], case);
        let publication = events[0].native_publication.unwrap();
        assert_eq!(
            publication.exact_pts_microseconds,
            Some(case.pts_microseconds)
        );
        assert_eq!(publication.start_display_ms, 0);
        assert_eq!(publication.end_display_ms, 2000);
        let palette = events[0].regions()[0].palette.native_d32.as_ref().unwrap();
        assert!(!palette.all_entries_explicit);
        assert_eq!(
            palette.words.as_slice(),
            case.native_words,
            "{} entire literal raw native words",
            case.id
        );
        assert_eq!(
            palette.rgba.as_ref(),
            case.native_rgba,
            "{} all entries, including unused/transparent RGB",
            case.id
        );
        drop(events);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn native_absent_cds_preserves_actual_automatic_palette_and_unused_entries() {
    native_case(0);
}

#[test]
fn native_partial_four_bit_palette_preserves_fixed_unredefined_entries() {
    native_case(1);
}

#[test]
fn native_partial_eight_bit_palette_preserves_all256_entries() {
    native_case(2);
}

#[test]
fn default_palette_and_original_owners_remain_charged_after_decoder_teardown() {
    for case in &cases::CASES {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let mut events = decode(&pool, case, ReceiverView::NativeD32, 1);
        let event = events.pop().unwrap();
        drop(events);
        let region = &event.regions()[0];
        let native = region.palette.native_d32.as_ref().unwrap();
        let expected = native.rgba.as_ref().to_vec();
        let held = [
            native.rgba.clone(),
            region.palette.rgba.clone(),
            region.plane.pixels.clone(),
        ];
        let packet = event.source_packets()[0].clone();
        drop(event);
        assert!(pool.used() > baseline);
        assert_eq!(held[0].as_ref(), expected);
        assert_eq!(held[1].as_ref(), source_palette(case));
        assert_eq!(held[2].as_ref(), case.indices);
        assert_eq!(packet.payload.as_ref(), case.payload);
        drop(held);
        assert!(pool.used() > baseline);
        drop(packet);
        assert_eq!(pool.used(), baseline);
    }
}
