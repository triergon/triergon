#![forbid(unsafe_code)]
use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, ColourSource, DecodeStatus, Decoder, Depth, DisplayEvent,
    DisplayLimits, PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};

struct Frame {
    sequence: u64,
    pts: i64,
    dts: i64,
    duration: i64,
    position: i64,
    payload: Vec<u8>,
    facts: Vec<[u8; 5]>,
    source_rgba: Vec<u8>,
    studio_rgba: Vec<u8>,
    plane: Vec<u8>,
    native: Vec<u32>,
}

struct Case {
    depth: Depth,
    frames: Vec<Frame>,
}

fn bytes<const N: usize>(input: &mut Cursor<&[u8]>) -> [u8; N] {
    let mut out = [0; N];
    input.read_exact(&mut out).unwrap();
    out
}

fn vector(input: &mut Cursor<&[u8]>, count: usize) -> Vec<u8> {
    let mut out = vec![0; count];
    input.read_exact(&mut out).unwrap();
    out
}

fn case(data: &[u8]) -> Case {
    let mut input = Cursor::new(data);
    assert_eq!(&bytes::<8>(&mut input), b"SMCE0001");
    let bits = bytes::<1>(&mut input)[0];
    let depth = match bits {
        2 => Depth::Two,
        4 => Depth::Four,
        8 => Depth::Eight,
        _ => panic!("authored depth"),
    };
    let count = usize::from(u16::from_be_bytes(bytes(&mut input)));
    let n = 1_usize << bits;
    let mut frames = Vec::new();
    for _ in 0..count {
        let sequence = u64::from_be_bytes(bytes(&mut input));
        let pts = i64::from_be_bytes(bytes(&mut input));
        let dts = i64::from_be_bytes(bytes(&mut input));
        let duration = i64::from_be_bytes(bytes(&mut input));
        let position = i64::from_be_bytes(bytes(&mut input));
        let length = usize::try_from(u32::from_be_bytes(bytes(&mut input))).unwrap();
        let payload = vector(&mut input, length);
        let facts = (0..n).map(|_| bytes(&mut input)).collect();
        let source_rgba = vector(&mut input, n * 4);
        let studio_rgba = vector(&mut input, n * 4);
        let plane = vector(&mut input, n * 2);
        let native = (0..n)
            .map(|_| u32::from_be_bytes(bytes(&mut input)))
            .collect();
        frames.push(Frame {
            sequence,
            pts,
            dts,
            duration,
            position,
            payload,
            facts,
            source_rgba,
            studio_rgba,
            plane,
            native,
        });
    }
    assert_eq!(usize::try_from(input.position()).unwrap(), data.len());
    Case { depth, frames }
}

fn configuration(pool: &Budget) -> SourceConfiguration {
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x10]).unwrap();
    SourceConfiguration::from_matroska_private(pool, &private, SourceClock::new(1, 1000).unwrap())
        .unwrap()
}

fn decode(
    pool: &Budget,
    case: &Case,
    view: ReceiverView,
    quantum: u64,
    pressure: bool,
) -> Result<(Vec<DisplayEvent>, u64), Error> {
    let cancel = AtomicBool::new(false);
    let configuration = configuration(pool);
    let mut decoder = Decoder::new_configured_with_receiver_view(
        pool,
        &configuration,
        17,
        DisplayLimits::default(),
        view,
        &cancel,
    )?;
    let mut events = Vec::new();
    let mut spent = 0_u64;
    let clock = |ticks: i64| {
        Some(ClockValue {
            ticks: i128::from(ticks),
            origin: ClockOrigin::Source,
        })
    };
    for frame in &case.frames {
        let payload = Bytes::copy_from_slice(pool, &frame.payload)?;
        let packet = configuration.packet(
            pool,
            &payload,
            PacketTiming {
                pts: clock(frame.pts),
                dts: clock(frame.dts),
                duration: clock(frame.duration),
            },
            PacketIdentity {
                generation: 17,
                sequence: frame.sequence,
                container_position: u64::try_from(frame.position).ok().map(|offset| {
                    SourceLocation {
                        source: SourceId(1),
                        offset,
                    }
                }),
            },
        )?;
        decoder.provide(&packet)?;
        let mut done = false;
        for _ in 0..3_000_000 {
            let blocker = if pressure {
                Some(pool.reserve(pool.available())?)
            } else {
                None
            };
            let mut work = WorkBudget::new(quantum);
            let mut progress = decoder.step(&mut work, &cancel)?;
            spent += work.spent();
            drop(blocker);
            if progress.status == DecodeStatus::NeedOutput {
                assert!(pressure);
                let mut work = WorkBudget::new(quantum);
                progress = decoder.step(&mut work, &cancel)?;
                spent += work.spent();
            }
            match progress.status {
                DecodeStatus::Yield => {}
                DecodeStatus::Event => events.push(progress.event.unwrap()),
                DecodeStatus::NeedPacket => {
                    done = true;
                    break;
                }
                _ => panic!("unexpected packet status {:?}", progress.status),
            }
        }
        assert!(done, "finite authored packet work");
    }
    decoder.finish_input()?;
    let progress = decoder.step(&mut WorkBudget::new(quantum), &cancel)?;
    assert_eq!(progress.status, DecodeStatus::Finished);
    Ok((events, spent))
}

fn assert_source(events: &[DisplayEvent], case: &Case) {
    assert_eq!(events.len(), case.frames.len());
    for (event, frame) in events.iter().zip(&case.frames) {
        assert_eq!(
            event.pts,
            Some(Time::new(i128::from(frame.pts), 1000).unwrap())
        );
        assert_eq!(event.source_packets().len(), 1);
        let packet = &event.source_packets()[0];
        assert_eq!(packet.payload.as_ref(), frame.payload);
        assert_eq!(packet.sequence, frame.sequence);
        let timing = packet.source.as_ref().unwrap().timing();
        assert_eq!(timing.pts.unwrap().ticks, i128::from(frame.pts));
        assert_eq!(timing.dts.unwrap().ticks, i128::from(frame.dts));
        assert_eq!(timing.duration.unwrap().ticks, i128::from(frame.duration));
        assert_eq!(event.regions().len(), 1);
        let region = &event.regions()[0];
        assert_eq!(region.palette.depth, case.depth);
        assert_eq!(region.plane.pixels.as_ref(), frame.plane);
        assert_eq!(region.palette.rgba.as_ref(), frame.source_rgba);
        assert_eq!(
            region.palette.studio_unrepaired_rgba.as_ref(),
            frame.studio_rgba
        );
        assert_eq!(region.palette.entries().len(), frame.facts.len());
        for (entry, &[full, y, cr, cb, transparency]) in
            region.palette.entries().iter().zip(&frame.facts)
        {
            assert_eq!(
                entry.source,
                ColourSource::YCrCbT {
                    y,
                    cr,
                    cb,
                    transparency,
                    full_resolution: full != 0
                }
            );
            let original = entry.original_segment.as_ref().unwrap().as_ref();
            assert!(frame.payload.windows(original.len()).any(|p| p == original));
        }
    }
}

fn assert_native(events: &[DisplayEvent], case: &Case) {
    assert_source(events, case);
    for (event, frame) in events.iter().zip(&case.frames) {
        let publication = event.receiver_publication.unwrap();
        assert_eq!(publication.exact_pts_microseconds, Some(frame.pts * 1000));
        assert_eq!(publication.start_display_ms, 0);
        assert_eq!(publication.end_display_ms, 2000);
        let native = event.regions()[0].palette.receiver.as_ref().unwrap();
        assert!(
            native.all_entries_explicit,
            "raw CDS provenance, not an equality certificate"
        );
        assert_eq!(native.words.as_slice(), frame.native);
        let expected: Vec<u8> = frame
            .native
            .iter()
            .flat_map(|word| {
                [
                    (word >> 16) as u8,
                    (word >> 8) as u8,
                    *word as u8,
                    (word >> 24) as u8,
                ]
            })
            .collect();
        assert_eq!(native.rgba.as_ref(), expected);
    }
}

fn native_guard(data: &[u8]) {
    let case = case(data);
    let mut previous_work = None;
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let (events, work) = decode(&pool, &case, ReceiverView::Receiver, quantum, false).unwrap();
        assert_native(&events, &case);
        if let Some(previous) = previous_work {
            assert_eq!(work, previous);
        }
        previous_work = Some(work);
        assert!(pool.used() > baseline);
        drop(events);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn full_rgb_rounding_and_clipping() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-rgb.case"));
}
#[test]
fn all_full_range_transparency_values() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-alpha.case"));
}
#[test]
fn unused_encoded_rgb_is_preserved() {
    native_guard(include_bytes!("explicit-clut-fixtures/unused-full.case"));
}
#[test]
fn full_range_two_bit_palette() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-depth2.case"));
}
#[test]
fn full_range_four_bit_palette() {
    native_guard(include_bytes!("explicit-clut-fixtures/full-depth4.case"));
}
#[test]
fn reduced_nonfinal_conversion() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/reduced-nonfinal.case"
    ));
}
#[test]
fn reduced_final_fresh_keeps_native_default() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/reduced-final-fresh.case"
    ));
}
#[test]
fn reduced_final_retains_prior_two_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail2.case"));
}
#[test]
fn reduced_final_retains_prior_four_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail4.case"));
}
#[test]
fn reduced_final_retains_prior_eight_bit_entry() {
    native_guard(include_bytes!("explicit-clut-fixtures/reduced-tail8.case"));
}
#[test]
fn native_yzero_companions_keep_raw_facts() {
    native_guard(include_bytes!(
        "explicit-clut-fixtures/transparent-companions.case"
    ));
}

#[test]
fn source_views_and_every_original_component_are_unchanged() {
    for data in [
        include_bytes!("explicit-clut-fixtures/full-rgb.case").as_slice(),
        include_bytes!("explicit-clut-fixtures/reduced-tail8.case").as_slice(),
    ] {
        let case = case(data);
        for quantum in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let (events, _) = decode(&pool, &case, ReceiverView::Source, quantum, false).unwrap();
            assert_source(&events, &case);
            assert!(
                events
                    .iter()
                    .all(|e| e.regions()[0].palette.receiver.is_none())
            );
            drop(events);
            assert_eq!(pool.used(), baseline);
        }
    }
}

#[test]
fn strict_source_rejects_yzero_nonzero_companions() {
    let pool = Budget::new(4 << 20).unwrap();
    let baseline = pool.used();
    let case = case(include_bytes!(
        "explicit-clut-fixtures/transparent-companions.case"
    ));
    assert_eq!(
        decode(&pool, &case, ReceiverView::Source, 1, false).unwrap_err(),
        Error::Invalid("DVB transparent source components")
    );
    assert_eq!(pool.used(), baseline);
}

#[test]
fn pressure_preserves_actual_work_output_and_retained_owners() {
    let case = case(include_bytes!("explicit-clut-fixtures/reduced-tail8.case"));
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let (plain, plain_work) =
            decode(&pool, &case, ReceiverView::Receiver, quantum, false).unwrap();
        assert_native(&plain, &case);
        let (pressured, work) =
            decode(&pool, &case, ReceiverView::Receiver, quantum, true).unwrap();
        assert_native(&pressured, &case);
        assert_eq!(plain_work, work);
        drop(plain);
        assert!(pool.used() > baseline);
        drop(pressured);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn cancellation_keeps_partial_native_ownership_bounded() {
    let case = case(include_bytes!("explicit-clut-fixtures/full-rgb.case"));
    let frame = &case.frames[0];
    for steps in [0, 512, 4096, 8192] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        {
            let cancel = AtomicBool::new(false);
            let configuration = configuration(&pool);
            let mut decoder = Decoder::new_configured_with_receiver_view(
                &pool,
                &configuration,
                17,
                DisplayLimits::default(),
                ReceiverView::Receiver,
                &cancel,
            )
            .unwrap();
            let payload = Bytes::copy_from_slice(&pool, &frame.payload).unwrap();
            let packet = configuration
                .packet(
                    &pool,
                    &payload,
                    PacketTiming {
                        pts: Some(ClockValue {
                            ticks: i128::from(frame.pts),
                            origin: ClockOrigin::Source,
                        }),
                        dts: None,
                        duration: None,
                    },
                    PacketIdentity {
                        generation: 17,
                        sequence: frame.sequence,
                        container_position: None,
                    },
                )
                .unwrap();
            decoder.provide(&packet).unwrap();
            for _ in 0..steps {
                let progress = decoder.step(&mut WorkBudget::new(1), &cancel).unwrap();
                assert_eq!(progress.status, DecodeStatus::Yield);
            }
            cancel.store(true, Ordering::Relaxed);
            assert_eq!(
                decoder.step(&mut WorkBudget::new(1), &cancel).unwrap_err(),
                Error::Cancelled
            );
        }
        assert_eq!(pool.used(), baseline);
    }
}
