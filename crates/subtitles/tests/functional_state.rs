//! Original source/native state regressions, authored before the receiver fix.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayAction, DisplayEvent, DisplayLimits,
    PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};

struct SourcePacket {
    body: &'static [u8],
    pts: i64,
}
struct Rect {
    geometry: [u32; 4],
    indices: &'static [u8],
    native_words: &'static [u32],
}
struct Expected {
    pts_us: i64,
    start: u32,
    end: u32,
    canvas: [u32; 2],
    rects: &'static [Rect],
}
struct Case {
    id: &'static str,
    packets: &'static [SourcePacket],
    events: &'static [Expected],
}
#[path = "functional-state-fixtures/cases.rs"]
mod cases;

fn run(index: usize) {
    let case = &cases::ALL[index];
    let mut ordinary_work = None;
    for (quantum, pressure) in [(1, false), (7, false), (4096, false), (1, true), (7, true)] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let private = Bytes::copy_from_slice(&pool, &[0, 1, 0, 2, 0x14]).unwrap();
        let config = SourceConfiguration::from_matroska_private(
            &pool,
            &private,
            SourceClock::new(1, 1000).unwrap(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = Decoder::new_configured_with_receiver_view(
            &pool,
            &config,
            71,
            DisplayLimits::default(),
            ReceiverView::NativeD32,
            &cancel,
        )
        .unwrap();
        let mut supplied = 0;
        let mut ended = false;
        let mut events = Vec::new();
        let mut error = None;
        let mut finished = false;
        let mut spent = 0_u64;
        let mut pauses = 0;
        for _ in 0..2_000_000 {
            let blocker = if pressure {
                Some(pool.reserve(pool.available()).unwrap())
            } else {
                None
            };
            let mut work = WorkBudget::new(quantum);
            let mut step = decoder.step(&mut work, &cancel);
            let mut step_spent = work.spent();
            drop(blocker);
            if step
                .as_ref()
                .is_ok_and(|p| p.status == DecodeStatus::NeedOutput)
            {
                assert!(pressure);
                pauses += 1;
                // The unblocked retry can validly request the next packet or
                // finish; it uses the same normal progress dispatcher below.
                let mut work = WorkBudget::new(quantum);
                step = decoder.step(&mut work, &cancel);
                step_spent = step_spent.checked_add(work.spent()).unwrap();
            }
            spent = spent.checked_add(step_spent).unwrap();
            let progress = match step {
                Ok(progress) => progress,
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            };
            match progress.status {
                DecodeStatus::NeedPacket => {
                    assert!(!ended);
                    if let Some(source) = case.packets.get(supplied) {
                        let payload = Bytes::copy_from_slice(&pool, source.body).unwrap();
                        let clock = Some(ClockValue {
                            ticks: i128::from(source.pts),
                            origin: ClockOrigin::Source,
                        });
                        let packet = config
                            .packet(
                                &pool,
                                &payload,
                                PacketTiming {
                                    pts: clock,
                                    dts: clock,
                                    duration: Some(ClockValue {
                                        ticks: 2000,
                                        origin: ClockOrigin::Source,
                                    }),
                                },
                                PacketIdentity {
                                    generation: 71,
                                    sequence: supplied as u64,
                                    container_position: None,
                                },
                            )
                            .unwrap();
                        decoder.provide(&packet).unwrap();
                        supplied += 1;
                    } else {
                        decoder.finish_input().unwrap();
                        ended = true;
                    }
                }
                DecodeStatus::Yield => assert!(step_spent > 0),
                DecodeStatus::Event => events.push(progress.event.unwrap()),
                DecodeStatus::NeedOutput => panic!("{} unblocked generous pool", case.id),
                DecodeStatus::Finished => {
                    finished = true;
                    break;
                }
            }
        }
        assert_eq!(
            error, None,
            "{} work={quantum} pressure={pressure}",
            case.id
        );
        assert!(finished && ended);
        assert_eq!(supplied, case.packets.len());
        drop(decoder);
        drop(config);
        drop(private);
        assert_eq!(events.len(), case.events.len(), "{}", case.id);
        let mut was_visible = false;
        let mut prior_epoch = None;
        for (actual, expected) in events.iter().zip(case.events) {
            assert_event(case, actual, expected);
            let visible = !expected.rects.is_empty();
            assert_eq!(
                actual.action,
                if !visible {
                    DisplayAction::Clear
                } else if was_visible && prior_epoch == Some(actual.epoch) {
                    DisplayAction::Update
                } else {
                    DisplayAction::Present
                }
            );
            was_visible = visible;
            prior_epoch = Some(actual.epoch);
        }
        if let Some(expected_work) = ordinary_work {
            assert_eq!(spent, expected_work, "{} work/pressure cursor", case.id);
        } else {
            ordinary_work = Some(spent);
        }
        if pressure {
            assert!(pauses > 0);
        }
        drop(events);
        assert_eq!(pool.used(), baseline, "{} retained-output release", case.id);
    }
}

fn assert_event(case: &Case, actual: &DisplayEvent, expected: &Expected) {
    assert_eq!(actual.receiver_view, ReceiverView::NativeD32);
    assert_eq!(
        [actual.definition.size.width, actual.definition.size.height],
        expected.canvas
    );
    assert_eq!(actual.generation, 71);
    assert_eq!(
        actual.pts,
        Some(Time::new(i128::from(expected.pts_us), 1_000_000).unwrap())
    );
    assert_eq!(
        actual.expiry,
        Some(
            actual
                .pts
                .unwrap()
                .checked_add(Time::new(2, 1).unwrap())
                .unwrap()
        )
    );
    let native = actual.native_publication.unwrap();
    assert_eq!(native.exact_pts_microseconds, Some(expected.pts_us));
    assert_eq!(
        [native.start_display_ms, native.end_display_ms],
        [expected.start, expected.end]
    );
    assert_eq!(
        actual.regions().len(),
        expected.rects.len(),
        "{} literal rectangle count",
        case.id
    );
    for (region, want) in actual.regions().iter().zip(expected.rects) {
        assert_eq!(
            [
                region.position.x,
                region.position.y,
                region.size.width,
                region.size.height
            ],
            want.geometry
        );
        assert_eq!(
            region.plane.geometry.stride,
            usize::try_from(want.geometry[2]).unwrap()
        );
        assert_eq!(region.plane.pixels.as_ref(), want.indices);
        let palette = region.palette.native_d32.as_ref().unwrap();
        assert_eq!(
            palette.words.as_slice(),
            want.native_words,
            "{} complete native palette",
            case.id
        );
        for (rgba, word) in palette.rgba.as_ref().chunks_exact(4).zip(want.native_words) {
            assert_eq!(
                rgba,
                [
                    (word >> 16) as u8,
                    (word >> 8) as u8,
                    *word as u8,
                    (word >> 24) as u8
                ]
            );
        }
        // Independent source colour facts and raw CDS owners remain available.
        assert!(!region.palette.entries().is_empty());
        for entry in region.palette.entries() {
            if let Some(original) = &entry.original_segment {
                assert!(original.as_ref().starts_with(&[15, 18]));
            }
        }
    }
    for packet in actual.source_packets() {
        let source = &case.packets[usize::try_from(packet.sequence).unwrap()];
        assert_eq!(packet.payload.as_ref(), source.body);
        let facts = packet.source.as_ref().unwrap();
        assert_eq!(
            facts.configuration().original().as_ref(),
            [0, 1, 0, 2, 0x14]
        );
        assert_eq!(facts.timing().pts.unwrap().ticks, i128::from(source.pts));
        assert_eq!(facts.timing().duration.unwrap().ticks, 2000);
    }
}

include!("functional-state-fixtures/names.rs");
