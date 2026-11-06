#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::compat::dvb::PublicationReason;
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, ColourSource, DecodeStatus, Decoder, DisplayEvent, DisplayLimits,
    PacketIdentity, PacketTiming, ReceiverView, Size, SourceClock, SourceConfiguration,
};
#[cfg(feature = "reference-trace")]
use triergon_subtitles::reference_trace::dvb::{
    RECEIVER_VIEW_PROFILE, REFERENCE_REVISION, ReceiverFailure,
};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    rgba: &'static [u8],
    indices: &'static [u8],
    words: &'static [u32],
    consumed: usize,
    pts_raw: i64,
    end_ms: u32,
    canvas: [u32; 2],
    geometry: [u32; 4],
    source_error: Option<Error>,
}
#[path = "native-dvb-fixtures/cases.rs"]
mod cases;
struct ResultView {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
    #[cfg(feature = "reference-trace")]
    receiver_failure: Option<ReceiverFailure>,
}

fn decode(pool: &Budget, bytes: &[u8], view: ReceiverView, quantum: u64) -> ResultView {
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
    let mut result = ResultView {
        events: Vec::new(),
        error: None,
        #[cfg(feature = "reference-trace")]
        receiver_failure: None,
    };
    let mut ended = false;
    for _ in 0..1_000_000 {
        let progress = match decoder.step(&mut WorkBudget::new(quantum), &cancel) {
            Ok(p) => p,
            Err(e) => {
                result.error = Some(e);
                #[cfg(feature = "reference-trace")]
                {
                    result.receiver_failure = decoder.receiver_failure();
                }
                return result;
            }
        };
        match progress.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => result.events.push(progress.event.unwrap()),
            DecodeStatus::NeedPacket => {
                assert!(!ended);
                decoder.finish_input().unwrap();
                ended = true;
            }
            DecodeStatus::NeedOutput => panic!("small exact case cannot exhaust pool"),
            DecodeStatus::Finished => return result,
        }
    }
    panic!("bounded source exceeded poll cap")
}
fn check(index: usize) {
    let case = &cases::CASES[index];
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let observed = decode(&pool, case.payload, ReceiverView::Receiver, quantum);
        assert_eq!(observed.error, None, "{} quantum={quantum}", case.id);
        assert_eq!(observed.events.len(), 1);
        #[cfg(feature = "reference-trace")]
        assert!(observed.receiver_failure.is_none());
        let event = &observed.events[0];
        assert_eq!(event.receiver_view, ReceiverView::Receiver);
        assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
        let publication = event.receiver_publication.unwrap();
        #[cfg(feature = "reference-trace")]
        assert_eq!(publication.reference_revision(), REFERENCE_REVISION);
        #[cfg(feature = "reference-trace")]
        assert_eq!(publication.view_profile(), RECEIVER_VIEW_PROFILE);
        assert_eq!(publication.consumed_prefix_bytes, case.consumed);
        assert_eq!(publication.packet_sequence, 0);
        assert_eq!(publication.reason, PublicationReason::EndOfDisplay);
        assert_eq!(publication.exact_pts_microseconds, Some(case.pts_raw));
        assert_eq!(publication.start_display_ms, 0);
        assert_eq!(publication.end_display_ms, case.end_ms);
        assert_eq!(
            publication.context_canvas,
            Some(Size {
                width: case.canvas[0],
                height: case.canvas[1]
            })
        );
        let (clock, timing) = publication.packet_clock.unwrap();
        assert_eq!(clock, SourceClock::new(1, 1000).unwrap());
        assert_eq!(timing.pts.unwrap().ticks, 1000);
        assert_eq!(timing.dts.unwrap().ticks, 1000);
        assert_eq!(
            [event.definition.size.width, event.definition.size.height],
            case.canvas
        );
        assert_eq!(event.source_packets().len(), 1);
        assert_eq!(event.source_packets()[0].payload.as_ref(), case.payload);
        let source = event.source_packets()[0].source.as_ref().unwrap();
        assert_eq!(source.timing().pts.unwrap().ticks, 1000);
        assert_eq!(source.timing().duration.unwrap().ticks, 2000);
        assert_eq!(event.regions().len(), 1);
        let region = &event.regions()[0];
        assert_eq!(
            [
                region.position.x,
                region.position.y,
                region.size.width,
                region.size.height
            ],
            case.geometry
        );
        assert_eq!(region.plane.pixels.as_ref(), case.indices);
        let native = region.palette.receiver.as_ref().unwrap();
        assert!(native.all_entries_explicit);
        assert_eq!(
            native.rgba.as_ref(),
            case.rgba,
            "entire native palette including unused transparent RGB"
        );
        assert_eq!(native.words.as_slice(), case.words);
        #[cfg(feature = "reference-trace")]
        assert_eq!(native.reference_revision(), REFERENCE_REVISION);
        #[cfg(feature = "reference-trace")]
        assert_eq!(native.view_profile(), RECEIVER_VIEW_PROFILE);
        assert_eq!(native.rgba.len(), case.words.len() * 4);
        // The source projection and every encoded CLUT component remain distinct.
        assert_eq!(&region.palette.rgba.as_ref()[..4], &[0, 0, 0, 0]);
        assert_eq!(
            region.palette.entries()[0].source,
            ColourSource::YCrCbT {
                y: 0,
                cr: 0,
                cb: 0,
                transparency: 0,
                full_resolution: true
            }
        );
        assert!(region.palette.entries()[0].original_segment.is_some());
        drop(observed);
        assert_eq!(pool.used(), baseline);
    }
}
#[test]
fn native_shared_ancillary_end() {
    check(0);
}
#[test]
fn native_nonempty_ancillary_end_tolerance() {
    check(1);
}
#[test]
fn native_composition_end_control() {
    check(2);
}
#[test]
fn native_shared_composition_end_tolerance() {
    check(3);
}
#[test]
fn native_foreign_end_filter_control() {
    check(4);
}

#[test]
fn source_profile_keeps_all_five_old_expectations_and_raw_colour() {
    for case in cases::CASES {
        let pool = Budget::new(4 << 20).unwrap();
        let observed = decode(&pool, case.payload, ReceiverView::Source, 7);
        assert_eq!(observed.error, case.source_error);
        assert_eq!(
            observed.events.len(),
            usize::from(case.source_error.is_none())
        );
        for event in observed.events {
            assert_eq!(event.receiver_view, ReceiverView::Source);
            assert!(event.receiver_publication.is_none());
            assert!(event.regions()[0].palette.receiver.is_none());
            assert_eq!(
                &event.regions()[0].palette.rgba.as_ref()[..4],
                &[0, 0, 0, 0]
            );
        }
    }
}
#[test]
fn native_original106_keeps_prefix_event_then_exact_unconsumed_five_byte_error() {
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let bytes = include_bytes!("native-dvb-fixtures/input/tail106.dvb");
        let observed = decode(&pool, bytes, ReceiverView::Receiver, quantum);
        assert_eq!(observed.events.len(), 1);
        assert_eq!(observed.error, Some(Error::Truncated));
        let publication = observed.events[0].receiver_publication.unwrap();
        assert_eq!(publication.reason, PublicationReason::CompletePrefix);
        assert_eq!(publication.consumed_prefix_bytes, 101);
        #[cfg(feature = "reference-trace")]
        {
            let error = observed.receiver_failure.unwrap();
            assert_eq!(error.packet_offset, 101);
            assert_eq!(error.unconsumed_bytes, 5);
            assert_eq!(error.code, -1094995529);
            assert_eq!(error.packet_sequence, 0);
        }
        assert_eq!(
            observed.events[0].source_packets()[0].payload.as_ref(),
            bytes
        );
        assert_eq!(bytes.len(), 106);
    }
}
#[test]
fn native_actual_ts101_prefix_needs_no_invented_eod_or_error() {
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let bytes = include_bytes!("native-dvb-fixtures/input/prefix101.dvb");
        let observed = decode(&pool, bytes, ReceiverView::Receiver, quantum);
        assert_eq!(observed.error, None);
        assert_eq!(observed.events.len(), 1);
        #[cfg(feature = "reference-trace")]
        assert!(observed.receiver_failure.is_none());
        let publication = observed.events[0].receiver_publication.unwrap();
        assert_eq!(publication.reason, PublicationReason::CompletePrefix);
        assert_eq!(publication.consumed_prefix_bytes, 101);
        assert_eq!(
            observed.events[0].source_packets()[0].payload.as_ref(),
            bytes
        );
    }
}
#[test]
fn source_tail_and_prefix_stay_truncated_without_publication() {
    for bytes in [
        &include_bytes!("native-dvb-fixtures/input/tail106.dvb")[..],
        &include_bytes!("native-dvb-fixtures/input/prefix101.dvb")[..],
    ] {
        let pool = Budget::new(4 << 20).unwrap();
        let observed = decode(&pool, bytes, ReceiverView::Source, 7);
        assert_eq!(observed.error, Some(Error::Truncated));
        assert!(observed.events.is_empty());
        #[cfg(feature = "reference-trace")]
        assert!(observed.receiver_failure.is_none());
    }
}
#[test]
fn native_palette_owner_alone_survives_decoder_event_and_configuration_drop() {
    let pool = Budget::new(4 << 20).unwrap();
    let baseline = pool.used();
    let observed = decode(&pool, cases::CASES[0].payload, ReceiverView::Receiver, 1);
    let rgba = observed.events[0].regions()[0]
        .palette
        .receiver
        .as_ref()
        .unwrap()
        .rgba
        .clone();
    drop(observed);
    assert_eq!(rgba.as_ref(), cases::CASES[0].rgba);
    assert_eq!(pool.used(), baseline + rgba.retained_storage_bytes());
    drop(rgba);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn native_default_clut_adaptation_gap_remains_visible() {
    let pool = Budget::new(4 << 20).unwrap();
    let observed = decode(
        &pool,
        include_bytes!("native-dvb-fixtures/input/default-clut2.dvb"),
        ReceiverView::Receiver,
        7,
    );
    assert_eq!(observed.error, None);
    assert_eq!(observed.events.len(), 1);
    assert!(
        !observed.events[0].regions()[0]
            .palette
            .receiver
            .as_ref()
            .unwrap()
            .all_entries_explicit
    );
}

#[test]
fn native_canvas_uses_first_definition_or_complete_packet_default() {
    let pool = Budget::new(4 << 20).unwrap();
    let original = cases::CASES[0].payload;
    assert_eq!(original[1], 0x14);
    let definition_end = 6 + usize::from(u16::from_be_bytes([original[4], original[5]]));
    let without = decode(
        &pool,
        &original[definition_end..],
        ReceiverView::Receiver,
        1,
    );
    assert_eq!(without.error, None);
    assert_eq!(
        without.events[0]
            .receiver_publication
            .unwrap()
            .context_canvas,
        Some(Size {
            width: 720,
            height: 576
        })
    );

    let mut changed = original[..definition_end].to_vec();
    changed[6] ^= 0x10;
    changed[7..11].copy_from_slice(&[0, 31, 0, 23]);
    let mut packet = original[..definition_end].to_vec();
    packet.extend_from_slice(&changed);
    packet.extend_from_slice(&original[definition_end..]);
    let later = decode(&pool, &packet, ReceiverView::Receiver, 7);
    assert_eq!(later.error, None);
    assert_eq!(
        later.events[0].definition.size,
        Size {
            width: 32,
            height: 24
        }
    );
    assert_eq!(
        later.events[0].receiver_publication.unwrap().context_canvas,
        Some(Size {
            width: cases::CASES[0].canvas[0],
            height: cases::CASES[0].canvas[1]
        })
    );
}
