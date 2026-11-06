#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, Depth, DisplayEvent, DisplayLimits,
    PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};
#[cfg(feature = "reference-trace")]
use triergon_subtitles::reference_trace::dvb::{RECEIVER_VIEW_PROFILE, REFERENCE_REVISION};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    source_indices: &'static [u8],
    source_palette: &'static [u8],
    native_indices: &'static [u8],
    native_palette: &'static [u8],
    receiver_words: &'static [u32],
    non_modifying: bool,
    depth: u8,
    pts_raw: i64,
}
#[path = "native-nonmodifying-support/cases.rs"]
mod cases;
struct Observed {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
}
fn decode(pool: &Budget, bytes: &[u8], view: ReceiverView, quantum: u64) -> Observed {
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14]).unwrap();
    let config = SourceConfiguration::from_matroska_private(
        pool,
        &private,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new_configured_with_receiver_view(
        pool,
        &config,
        13,
        DisplayLimits::default(),
        view,
        &cancel,
    )
    .unwrap();
    let raw = Bytes::copy_from_slice(pool, bytes).unwrap();
    let packet = config
        .packet(
            pool,
            &raw,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                dts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                duration: Some(ClockValue {
                    ticks: 2000,
                    origin: ClockOrigin::Source,
                }),
            },
            PacketIdentity {
                generation: 13,
                sequence: 0,
                container_position: None,
            },
        )
        .unwrap();
    decoder.provide(&packet).unwrap();
    let mut observed = Observed {
        events: Vec::new(),
        error: None,
    };
    let mut ended = false;
    for _ in 0..1_000_000 {
        let progress = match decoder.step(&mut WorkBudget::new(quantum), &cancel) {
            Ok(progress) => progress,
            Err(error) => {
                observed.error = Some(error);
                return observed;
            }
        };
        match progress.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => observed.events.push(progress.event.unwrap()),
            DecodeStatus::NeedPacket => {
                assert!(!ended);
                decoder.finish_input().unwrap();
                ended = true;
            }
            DecodeStatus::NeedOutput => panic!("small full-CLUT input exhausted4MiB"),
            DecodeStatus::Finished => return observed,
        }
    }
    panic!("bounded input exceeded poll cap")
}
fn check_native(index: usize) {
    let case = &cases::CASES[index];
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let observed = decode(&pool, case.payload, ReceiverView::Receiver, quantum);
        assert_eq!(observed.error, None, "{} work={quantum}", case.id);
        assert_eq!(observed.events.len(), 1);
        let event = &observed.events[0];
        assert_eq!(event.receiver_view, ReceiverView::Receiver);
        assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(
            [event.definition.size.width, event.definition.size.height],
            [16, 12]
        );
        assert_eq!(event.source_packets().len(), 1);
        assert_eq!(event.source_packets()[0].payload.as_ref(), case.payload);
        let native = event.receiver_publication.unwrap();
        #[cfg(feature = "reference-trace")]
        assert_eq!(native.view_profile(), RECEIVER_VIEW_PROFILE);
        #[cfg(feature = "reference-trace")]
        assert_eq!(native.reference_revision(), REFERENCE_REVISION);
        assert_eq!(native.exact_pts_microseconds, Some(case.pts_raw));
        assert_eq!(native.start_display_ms, 0);
        assert_eq!(native.end_display_ms, 2000);
        assert_eq!(native.consumed_prefix_bytes, case.payload.len());
        assert_eq!(event.regions().len(), 1);
        let region = &event.regions()[0];
        assert_eq!(
            [
                region.position.x,
                region.position.y,
                region.size.width,
                region.size.height
            ],
            [1, 1, 5, 2]
        );
        assert_eq!(region.plane.geometry.stride, 5);
        assert_eq!(
            region.plane.depth,
            match case.depth {
                2 => Depth::Two,
                4 => Depth::Four,
                8 => Depth::Eight,
                _ => panic!("fixture depth"),
            }
        );
        let palette = region.palette.receiver.as_ref().unwrap();
        assert!(palette.all_entries_explicit);
        assert_eq!(
            palette.rgba.as_ref(),
            case.native_palette,
            "entire literal native palette"
        );
        assert_eq!(palette.words.as_slice(), case.receiver_words);
        assert_eq!(palette.words.len(), 1_usize << case.depth);
        assert_eq!(
            region.palette.rgba.as_ref(),
            case.source_palette,
            "source colour projection unchanged"
        );
        assert_eq!(
            region.plane.pixels.as_ref(),
            case.native_indices,
            "{} literal native cursor plane, flag={}",
            case.id,
            case.non_modifying
        );
        drop(observed);
        assert_eq!(pool.used(), baseline);
    }
}
include!("native-nonmodifying-support/names.rs");

#[test]
fn original_twelve_source_planes_and_palettes_are_unchanged() {
    for case in cases::CASES {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(&pool, case.payload, ReceiverView::Source, quantum);
            assert_eq!(observed.error, None);
            assert_eq!(observed.events.len(), 1);
            let event = &observed.events[0];
            assert!(event.receiver_publication.is_none());
            assert_eq!(event.source_packets()[0].payload.as_ref(), case.payload);
            let region = &event.regions()[0];
            assert!(region.palette.receiver.is_none());
            assert_eq!(region.plane.pixels.as_ref(), case.source_indices);
            assert_eq!(region.palette.rgba.as_ref(), case.source_palette);
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
#[test]
fn source_mapping_rule_and_all_original_map_bytes_are_preserved() {
    for (input, indices) in [
        (
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to4-raw1-to2-raw2-to1.dvb"
            )[..],
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to4-raw1-to2-raw2-to1.source-indices"
            )[..],
        ),
        (
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to8-raw1-to2-raw2-to1.dvb"
            )[..],
            &include_bytes!(
                "native-nonmodifying-fixtures/mapped-controls/map2to8-raw1-to2-raw2-to1.source-indices"
            )[..],
        ),
    ] {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(&pool, input, ReceiverView::Source, quantum);
            assert_eq!(observed.error, None);
            assert_eq!(observed.events.len(), 1);
            assert_eq!(
                observed.events[0].source_packets()[0].payload.as_ref(),
                input
            );
            assert_eq!(
                observed.events[0].regions()[0].plane.pixels.as_ref(),
                indices
            );
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
#[test]
fn native_compaction_must_not_hide_logical_source_row_overrun() {
    for view in [ReceiverView::Source, ReceiverView::Receiver] {
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let observed = decode(
                &pool,
                include_bytes!("native-nonmodifying-fixtures/logical-width-overrun.dvb"),
                view,
                quantum,
            );
            assert_eq!(
                observed.error,
                Some(Error::Invalid("DVB run exceeds source row"))
            );
            assert!(observed.events.is_empty());
            drop(observed);
            assert_eq!(pool.used(), baseline);
        }
    }
}
