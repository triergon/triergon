use std::{cell::RefCell, rc::Rc, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, Error as CoreError, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_framing::{Codec, RawAnnexBLimits, RawAnnexBReader};
use triergon_playback::{ByteSource, Error, Source, SourceReply, raw_video::RawVideoSource};

const SOURCE: SourceId = SourceId(72);
static OFF: AtomicBool = AtomicBool::new(false);
// One HEVC IRAP NAL with a first-slice flag. Its source EOF is independently controlled.
const INPUT: &[u8] = &[0, 0, 0, 1, 19 << 1, 1, 0x80, 0x55];

#[derive(Default)]
struct Calls {
    reads: usize,
    ends: usize,
    abort: usize,
    close: usize,
}

struct Controlled {
    bytes: ByteSource,
    empty: Bytes,
    calls: Rc<RefCell<Calls>>,
    tail: Rc<RefCell<Tail>>,
}

#[derive(Clone, Copy)]
enum Tail {
    Pending,
    Empty,
    End,
    Failed,
}

impl Source for Controlled {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        self.calls.borrow_mut().reads += 1;
        if request.offset < INPUT.len() as u64 {
            return self.bytes.poll(request, work, cancel);
        }
        if !work.try_charge(1) {
            return Ok(SourceReply::Pending);
        }
        Ok(match *self.tail.borrow() {
            Tail::Pending => SourceReply::Pending,
            Tail::Empty => SourceReply::Data(InputWindow::new(
                request.source,
                request.token.clone(),
                request.offset,
                self.empty.clone(),
            )?),
            Tail::End => SourceReply::End {
                source: request.source,
                token: request.token.clone(),
            },
            Tail::Failed => SourceReply::Failed {
                source: request.source,
                token: request.token.clone(),
                failure: SourceFailure::Transport,
            },
        })
    }
    fn invalidate(&mut self) {
        panic!("raw sequential source must not seek");
    }
    fn abort(&mut self) {
        self.calls.borrow_mut().abort += 1;
    }
    fn close(&mut self) {
        self.calls.borrow_mut().close += 1;
    }
}

fn source(
    pool: &Budget,
    calls: Rc<RefCell<Calls>>,
    tail: Rc<RefCell<Tail>>,
) -> RawVideoSource<Controlled> {
    let reader = RawAnnexBReader::new(
        pool,
        Codec::Hevc,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        RawAnnexBLimits::default(),
    )
    .unwrap();
    let input = Bytes::copy_from_slice(pool, INPUT).unwrap();
    let empty = input.slice(0..0).unwrap();
    let bytes = ByteSource::new(SOURCE, input, 2).unwrap();
    RawVideoSource::new(
        reader,
        Controlled {
            bytes,
            empty,
            calls,
            tail,
        },
    )
}

#[test]
fn raw_source_distinguishes_empty_reads_from_eof_and_closes_once() {
    let pool = Budget::new(1 << 20).unwrap();
    let calls = Rc::new(RefCell::new(Calls::default()));
    let tail = Rc::new(RefCell::new(Tail::Pending));
    let mut source = source(&pool, calls.clone(), tail.clone());
    let Step::NeedInput(pending) = source.poll(&mut WorkBudget::new(512), &OFF).unwrap() else {
        panic!("unconfirmed tail published output");
    };
    *tail.borrow_mut() = Tail::Empty;
    let Step::NeedInput(empty) = source.poll(&mut WorkBudget::new(512), &OFF).unwrap() else {
        panic!("empty tail published output");
    };
    assert_eq!(pending.offset, INPUT.len() as u64);
    assert_eq!(empty.offset, pending.offset);
    assert_eq!(empty.token, pending.token);

    *tail.borrow_mut() = Tail::End;
    let Step::Ready(packet) = source.poll(&mut WorkBudget::new(512), &OFF).unwrap() else {
        panic!("confirmed tail did not publish the last access unit");
    };
    assert!(packet.random_access);
    assert!(
        packet
            .segments()
            .flatten()
            .copied()
            .eq(INPUT.iter().copied())
    );
    for _ in 0..2 {
        assert!(matches!(
            source.poll(&mut WorkBudget::new(512), &OFF).unwrap(),
            Step::End(at) if at.offset == INPUT.len() as u64
        ));
    }
    drop(source);
    let calls = calls.borrow();
    assert_eq!((calls.abort, calls.close), (1, 1));
    drop(packet);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn raw_source_transport_failure_and_cancellation_retire_partial_state() {
    for expected in [CoreError::Transport, CoreError::Cancelled] {
        let pool = Budget::new(1 << 20).unwrap();
        let calls = Rc::new(RefCell::new(Calls::default()));
        let tail = Rc::new(RefCell::new(Tail::Pending));
        let mut source = source(&pool, calls.clone(), tail.clone());
        assert!(matches!(
            source.poll(&mut WorkBudget::new(512), &OFF).unwrap(),
            Step::NeedInput(_)
        ));
        *tail.borrow_mut() = Tail::Failed;
        let cancel = AtomicBool::new(expected == CoreError::Cancelled);
        for _ in 0..2 {
            assert!(matches!(
                source.poll(&mut WorkBudget::new(512), &cancel),
                Err(Error::Core(error)) if error == expected
            ));
        }
        assert_eq!(pool.used(), Budget::control_storage_bytes());
        drop(source);
        let calls = calls.borrow();
        assert_eq!((calls.abort, calls.close), (1, 1));
    }
}

struct Sequential {
    bytes: ByteSource,
    cursor: Rc<RefCell<u64>>,
    reads: Rc<RefCell<Vec<(u64, usize)>>>,
    counts: Rc<RefCell<Calls>>,
}

impl Source for Sequential {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        assert_eq!(
            request.offset,
            *self.cursor.borrow(),
            "probe rewound a forward source"
        );
        let reply = self.bytes.poll(request, work, cancel)?;
        if let SourceReply::Data(window) = &reply {
            let bytes = window.bytes().len();
            self.reads.borrow_mut().push((request.offset, bytes));
            *self.cursor.borrow_mut() += bytes as u64;
        } else if matches!(reply, SourceReply::End { .. }) {
            let mut counts = self.counts.borrow_mut();
            counts.ends += 1;
            assert_eq!(counts.ends, 1, "confirmed source EOF was requested twice");
        }
        Ok(reply)
    }

    fn invalidate(&mut self) {
        panic!("sequential raw detection must not invalidate a delivered request");
    }
    fn abort(&mut self) {
        self.counts.borrow_mut().abort += 1;
    }
    fn close(&mut self) {
        self.counts.borrow_mut().close += 1;
    }
}

const RAW_FIXTURES: [(Codec, &[u8]); 4] = [
    (
        Codec::Avc,
        include_bytes!("../../framing/fixtures/raw-annex-b/avc-aud0.h264"),
    ),
    (
        Codec::Avc,
        include_bytes!("../../framing/fixtures/raw-annex-b/avc-aud1.h264"),
    ),
    (
        Codec::Hevc,
        include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud0.hevc"),
    ),
    (
        Codec::Hevc,
        include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud1.hevc"),
    ),
];

#[test]
fn raw_probe_preserves_all_bytes_from_forward_only_sources() {
    use triergon_playback::raw_video::{RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep};
    for (codec, bytes) in RAW_FIXTURES {
        for (quantum, work) in [(1, 1), (7, 31), (4096, 1), (4096, 4096)] {
            let pool = Budget::new(1 << 20).unwrap();
            let cursor = Rc::new(RefCell::new(0));
            let reads = Rc::new(RefCell::new(Vec::new()));
            let counts = Rc::new(RefCell::new(Calls::default()));
            let source = Sequential {
                bytes: ByteSource::new(
                    SOURCE,
                    Bytes::copy_from_slice(&pool, bytes).unwrap(),
                    quantum,
                )
                .unwrap(),
                cursor: cursor.clone(),
                reads: reads.clone(),
                counts: counts.clone(),
            };
            let mut probe = RawVideoProbe::new(
                &pool,
                SOURCE,
                SourceCapabilities {
                    length: None,
                    random_access: false,
                },
                source,
                RawVideoProbeLimits::default(),
                RawAnnexBLimits::default(),
            )
            .unwrap();
            let detected = (0..100_000)
                .find_map(
                    |_| match probe.advance(&mut WorkBudget::new(work), &OFF).unwrap() {
                        RawVideoProbeStep::Detected(codec) => Some(codec),
                        RawVideoProbeStep::NeedInput(_) | RawVideoProbeStep::Yield => None,
                    },
                )
                .unwrap();
            assert_eq!(detected, codec);
            let handoff = *cursor.borrow();
            assert!(handoff > 0 && handoff < bytes.len() as u64);

            let mut source = probe.finish().unwrap();
            assert!(matches!(probe.finish(), Err(Error::Closed)));
            drop(probe);
            let mut count = 0;
            let mut end = 0;
            for _ in 0..100_000 {
                match source.poll(&mut WorkBudget::new(work), &OFF).unwrap() {
                    Step::Ready(unit) => {
                        assert_eq!(unit.codec, codec);
                        assert_eq!(unit.range.start, end);
                        let wanted = &bytes[unit.range.start as usize..unit.range.end as usize];
                        assert!(
                            unit.segments()
                                .flatten()
                                .copied()
                                .eq(wanted.iter().copied())
                        );
                        end = unit.range.end;
                        count += 1;
                    }
                    Step::End(at) => {
                        assert_eq!(at.offset, bytes.len() as u64);
                        break;
                    }
                    Step::NeedInput(_) | Step::Yield => {}
                }
            }
            assert_eq!(count, 12);
            assert_eq!(end, bytes.len() as u64);
            assert_eq!(*cursor.borrow(), end);
            assert_eq!(
                reads.borrow().iter().map(|&(_, len)| len).sum::<usize>(),
                bytes.len()
            );
            drop(source);
            assert_eq!((counts.borrow().abort, counts.borrow().close), (1, 1));
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn raw_probe_refuses_mixed_evidence_and_uses_explicit_scan_bounds() {
    use triergon_playback::raw_video::{RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep};
    let mixed: &[u8] = &[
        0, 0, 1, 0x67, 100, 0, 31, 0x80, 0, 0, 1, 0x68, 0x80, 0, 0, 1, 0x40, 1, 0x80, 0, 0, 1,
        0x42, 1, 1, 0x80, 0, 0, 1, 0x44, 2, 0x80, 0, 0, 1, 0x46, 1, 0x80,
    ];
    for (bytes, limits, expected) in [
        (
            mixed,
            RawVideoProbeLimits::default(),
            CoreError::Invalid("ambiguous raw Annex B parameter-set signatures"),
        ),
        (
            RAW_FIXTURES[0].1,
            RawVideoProbeLimits {
                maximum_bytes: 8,
                maximum_nals: 32,
            },
            CoreError::Unsupported("raw video signature absent within probe bound"),
        ),
        (
            RAW_FIXTURES[0].1,
            RawVideoProbeLimits {
                maximum_bytes: 65536,
                maximum_nals: 1,
            },
            CoreError::Limit {
                requested: 2,
                maximum: 1,
            },
        ),
        (
            &[0, 0, 1, 0x09, 0x10][..],
            RawVideoProbeLimits::default(),
            CoreError::Unsupported("raw video signature absent at EOF"),
        ),
    ] {
        let pool = Budget::new(1 << 20).unwrap();
        let input =
            ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, bytes).unwrap(), 7).unwrap();
        let mut probe = RawVideoProbe::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            input,
            limits,
            RawAnnexBLimits::default(),
        )
        .unwrap();
        let error = (0..10_000)
            .find_map(|_| match probe.advance(&mut WorkBudget::new(13), &OFF) {
                Err(Error::Core(error)) => Some(error),
                Ok(RawVideoProbeStep::NeedInput(_) | RawVideoProbeStep::Yield) => None,
                other => panic!("unexpected raw probe result {other:?}"),
            })
            .unwrap();
        assert_eq!(error, expected);
        assert!(matches!(
            probe.advance(&mut WorkBudget::new(13), &OFF),
            Err(Error::Closed)
        ));
    }
}

#[test]
fn auto_detected_raw_streams_resolve_exact_active_decoder_parameter_sets() {
    use triergon_playback::raw_video::{
        AvcVuiTiming, RawVideoConfigTracker, RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep,
    };
    for (codec, bytes) in RAW_FIXTURES {
        let pool = Budget::new(1 << 20).unwrap();
        let input =
            ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, bytes).unwrap(), 19).unwrap();
        let mut probe = RawVideoProbe::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: Some(bytes.len() as u64),
                random_access: false,
            },
            input,
            RawVideoProbeLimits::default(),
            RawAnnexBLimits::default(),
        )
        .unwrap();
        let selected = loop {
            if let RawVideoProbeStep::Detected(codec) =
                probe.advance(&mut WorkBudget::new(127), &OFF).unwrap()
            {
                break codec;
            }
        };
        assert_eq!(selected, codec);
        let mut raw = probe.finish().unwrap();
        let mut tracking = RawVideoConfigTracker::new(&pool, selected, 4096).unwrap();
        let mut active = None;
        let mut count = 0;
        for _ in 0..5000 {
            match raw.poll(&mut WorkBudget::new(127), &OFF).unwrap() {
                Step::Ready(unit) => {
                    let config = tracking.observe(&unit).unwrap().unwrap();
                    assert_eq!(config.codec, codec);
                    let (dimensions, kinds): (_, &[u8]) = match codec {
                        Codec::Avc => ((64, 64), &[7, 8]),
                        Codec::Hevc => ((160, 96), &[32, 33, 34]),
                    };
                    assert_eq!((config.width, config.height), dimensions);
                    let timing = (codec == Codec::Avc).then_some(AvcVuiTiming {
                        num_units_in_tick: 1,
                        time_scale: 16,
                        fixed_frame_rate_flag: false,
                    });
                    assert_eq!(config.avc_timing, timing);
                    assert_eq!(config.parameter_sets().count(), kinds.len());
                    if count == 0 {
                        for (set, &kind) in config.parameter_sets().zip(kinds) {
                            let nal = unit
                                .nals()
                                .iter()
                                .find(|nal| nal.facts().header.nal_unit_type() == kind)
                                .unwrap();
                            assert!(std::ptr::eq(set.as_ref(), nal.bytes().as_ref()));
                        }
                        active = Some(config);
                    } else {
                        assert_eq!(Some(&config), active.as_ref());
                    }
                    count += 1;
                }
                Step::End(_) => break,
                Step::NeedInput(_) | Step::Yield => {}
            }
        }
        assert_eq!(count, 12);
        drop(active);
        drop(tracking);
        drop(raw);
        drop(probe);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn raw_configuration_switches_on_reused_ids_and_old_owners_remain_valid() {
    use triergon_playback::raw_video::RawVideoConfigTracker;
    let original = RAW_FIXTURES[0].1;
    let suffix = include_bytes!("fixtures/raw-video/avc-80x48.h264");
    let bytes = [original, suffix].concat();
    let pool = Budget::new(1 << 20).unwrap();
    let reader = RawAnnexBReader::new(
        &pool,
        Codec::Avc,
        SOURCE,
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        RawAnnexBLimits::default(),
    )
    .unwrap();
    let input =
        ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, &bytes).unwrap(), 79).unwrap();
    let mut source = RawVideoSource::new(reader, input);
    let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Avc, 4096).unwrap();
    let mut first = None;
    let mut counts = [0, 0];
    for _ in 0..10_000 {
        match source.poll(&mut WorkBudget::new(53), &OFF).unwrap() {
            Step::Ready(unit) => {
                let active = tracker.observe(&unit).unwrap().unwrap();
                let second = unit.range.start >= original.len() as u64;
                counts[usize::from(second)] += 1;
                if second {
                    assert_eq!((active.width, active.height), (80, 48));
                    let first: &triergon_playback::raw_video::RawVideoConfiguration =
                        first.as_ref().unwrap();
                    assert_eq!((first.width, first.height), (64, 64));
                    assert_ne!(first.sps.as_ref(), active.sps.as_ref());
                    assert!(
                        original
                            .windows(first.sps.len())
                            .any(|window| window == first.sps.as_ref())
                    );
                } else {
                    assert_eq!((active.width, active.height), (64, 64));
                    first.get_or_insert(active);
                }
            }
            Step::End(_) => break,
            Step::NeedInput(_) | Step::Yield => {}
        }
    }
    assert_eq!(counts, [12, 2]);
}

#[test]
fn raw_probe_backpressure_retains_the_delivered_source_window() {
    use triergon_playback::raw_video::{RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep};
    let (codec, bytes) = RAW_FIXTURES[0];
    let pool = Budget::new(1 << 20).unwrap();
    let cursor = Rc::new(RefCell::new(0));
    let reads = Rc::new(RefCell::new(Vec::new()));
    let counts = Rc::new(RefCell::new(Calls::default()));
    let source = Sequential {
        bytes: ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, bytes).unwrap(), 4096)
            .unwrap(),
        cursor: cursor.clone(),
        reads: reads.clone(),
        counts: counts.clone(),
    };
    let mut probe = RawVideoProbe::new(
        &pool,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        source,
        RawVideoProbeLimits::default(),
        RawAnnexBLimits::default(),
    )
    .unwrap();
    let held = pool.reserve(pool.limit() - pool.used()).unwrap();
    assert!(matches!(
        probe.advance(&mut WorkBudget::new(8192), &OFF),
        Err(Error::Core(CoreError::Backpressure { .. }))
    ));
    assert_eq!(reads.borrow().as_slice(), &[(0, 4096)]);
    drop(held);
    assert!(matches!(
        probe.advance(&mut WorkBudget::new(8192), &OFF).unwrap(),
        RawVideoProbeStep::Detected(selected) if selected == codec
    ));
    assert_eq!(reads.borrow().as_slice(), &[(0, 4096)]);
    drop(probe.finish().unwrap());
    drop(probe);
    assert_eq!((counts.borrow().abort, counts.borrow().close), (1, 1));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn playback_probe_selects_containers_and_replays_the_exact_forward_prefix() {
    use triergon_mpegts::PacketFormat;
    use triergon_playback::{
        Format,
        io::{PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, provide},
        raw_video::RawVideoProbeLimits,
    };
    let inputs: [(Format, &[u8], u64); 4] = [
        (
            Format::Matroska,
            include_bytes!("fixtures/mkv-feed/three-tracks.mkv"),
            4,
        ),
        (
            Format::Mp4,
            include_bytes!("fixtures/mp4/encoded-avc-aac-front.mp4"),
            8,
        ),
        (
            Format::Ts(PacketFormat::Ts188),
            include_bytes!("fixtures/ts/ts188-basic.ts"),
            389,
        ),
        (
            Format::Ts(PacketFormat::M2ts192),
            include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts"),
            389,
        ),
    ];
    for (format, bytes, probe_end) in inputs {
        for (quantum, work) in [(1, 1), (13, 19), (4096, 4096)] {
            let pool = Budget::new(1 << 20).unwrap();
            let cursor = Rc::new(RefCell::new(0));
            let counts = Rc::new(RefCell::new(Calls::default()));
            let source = Sequential {
                bytes: ByteSource::new(
                    SOURCE,
                    Bytes::copy_from_slice(&pool, bytes).unwrap(),
                    quantum,
                )
                .unwrap(),
                cursor: cursor.clone(),
                reads: Rc::new(RefCell::new(Vec::new())),
                counts: counts.clone(),
            };
            let mut probe = PlaybackProbe::new(
                &pool,
                SOURCE,
                SourceCapabilities {
                    length: None,
                    random_access: false,
                },
                4096,
                source,
                RawVideoProbeLimits::default(),
            )
            .unwrap();
            let detected = (0..20_000).find_map(|_| {
                match probe.advance(&mut WorkBudget::new(work), &OFF).unwrap() {
                    PlaybackProbeStep::Detected(kind) => Some(kind),
                    PlaybackProbeStep::NeedInput(_) | PlaybackProbeStep::Yield => None,
                }
            });
            assert_eq!(detected, Some(PlaybackInputKind::Container(format)));
            assert_eq!(*cursor.borrow(), probe_end);
            let PlaybackInput::Container(mut selected) =
                probe.finish(RawAnnexBLimits::default()).unwrap()
            else {
                panic!("container probe did not produce an opener input");
            };
            assert_eq!(selected.format, format);
            assert_eq!(selected.input.position(), 0);
            drop(probe);

            let mut compared = 0;
            for _ in 0..100_000 {
                let mut work = WorkBudget::new(work);
                match selected.input.read_span(4096, &mut work, &OFF).unwrap() {
                    Step::Ready(span) => {
                        let end = compared + span.len();
                        assert_eq!(span.as_ref(), &bytes[compared..end]);
                        compared = end;
                    }
                    Step::NeedInput(request) => {
                        let reply = selected.source.poll(&request, &mut work, &OFF).unwrap();
                        provide(&mut selected.input, &reply).unwrap();
                    }
                    Step::Yield => {}
                    Step::End(at) => {
                        assert_eq!(at.offset, bytes.len() as u64);
                        break;
                    }
                }
            }
            assert_eq!(compared, bytes.len());
            assert_eq!(*cursor.borrow(), bytes.len() as u64);
            assert_eq!(counts.borrow().ends, 1);
            drop(selected);
            assert_eq!((counts.borrow().abort, counts.borrow().close), (1, 1));
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn playback_probe_hands_off_raw_video_and_preserves_already_observed_eof() {
    use triergon_playback::{
        io::{PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep},
        raw_video::RawVideoProbeLimits,
    };
    let short = include_bytes!("fixtures/raw-video/avc-short.h264");
    for (codec, bytes, eof_at_selection, expected_packets) in [
        (Codec::Avc, short.as_slice(), true, 1),
        (RAW_FIXTURES[0].0, RAW_FIXTURES[0].1, false, 12),
        (RAW_FIXTURES[2].0, RAW_FIXTURES[2].1, false, 12),
    ] {
        let pool = Budget::new(1 << 20).unwrap();
        let cursor = Rc::new(RefCell::new(0));
        let counts = Rc::new(RefCell::new(Calls::default()));
        let source = Sequential {
            bytes: ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, bytes).unwrap(), 4096)
                .unwrap(),
            cursor: cursor.clone(),
            reads: Rc::new(RefCell::new(Vec::new())),
            counts: counts.clone(),
        };
        let mut probe = PlaybackProbe::new(
            &pool,
            SOURCE,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            4096,
            source,
            RawVideoProbeLimits::default(),
        )
        .unwrap();
        let detected =
            (0..50_000).find_map(
                |_| match probe.advance(&mut WorkBudget::new(1), &OFF).unwrap() {
                    PlaybackProbeStep::Detected(kind) => Some(kind),
                    PlaybackProbeStep::NeedInput(_) | PlaybackProbeStep::Yield => None,
                },
            );
        assert_eq!(detected, Some(PlaybackInputKind::RawVideo(codec)));
        assert_eq!(counts.borrow().ends, usize::from(eof_at_selection));
        let PlaybackInput::RawVideo {
            codec: selected,
            mut source,
        } = probe.finish(RawAnnexBLimits::default()).unwrap()
        else {
            panic!("raw source was not opened");
        };
        assert_eq!(selected, codec);
        drop(probe);
        let mut compared = 0;
        let mut count = 0;
        for _ in 0..100_000 {
            match source.poll(&mut WorkBudget::new(31), &OFF).unwrap() {
                Step::Ready(unit) => {
                    assert_eq!(unit.range.start as usize, compared);
                    let end = unit.range.end as usize;
                    assert!(
                        unit.segments()
                            .flatten()
                            .copied()
                            .eq(bytes[compared..end].iter().copied())
                    );
                    compared = end;
                    count += 1;
                }
                Step::End(at) => {
                    assert_eq!(at.offset, bytes.len() as u64);
                    break;
                }
                Step::NeedInput(_) | Step::Yield => {}
            }
        }
        assert_eq!(compared, bytes.len());
        assert_eq!(count, expected_packets);
        assert_eq!(counts.borrow().ends, 1);
        drop(source);
        assert_eq!((counts.borrow().abort, counts.borrow().close), (1, 1));
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn playback_probe_rejects_conflicting_transport_signatures() {
    use triergon_playback::{
        io::{PlaybackProbe, PlaybackProbeStep},
        raw_video::RawVideoProbeLimits,
    };
    let mut bytes = [0; 389];
    for at in [0, 188, 376, 4, 196, 388] {
        bytes[at] = 0x47;
    }
    let pool = Budget::new(1 << 20).unwrap();
    let source =
        ByteSource::new(SOURCE, Bytes::copy_from_slice(&pool, &bytes).unwrap(), 17).unwrap();
    let mut probe = PlaybackProbe::new(
        &pool,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        64,
        source,
        RawVideoProbeLimits::default(),
    )
    .unwrap();
    let err = (0..1000).find_map(|_| match probe.advance(&mut WorkBudget::new(43), &OFF) {
        Err(Error::Core(error)) => Some(error),
        Ok(PlaybackProbeStep::NeedInput(_) | PlaybackProbeStep::Yield) => None,
        other => panic!("unexpected ambiguous probe outcome {other:?}"),
    });
    assert_eq!(
        err,
        Some(CoreError::Invalid("ambiguous transport signature"))
    );
    assert!(matches!(
        probe.advance(&mut WorkBudget::new(43), &OFF),
        Err(Error::Closed)
    ));
}

fn open_raw_feed(
    pool: &Budget,
    codec: Codec,
    bytes: &[u8],
    clock: triergon_playback::raw_video::RawFeedClock,
) -> triergon_playback::raw_video::RawVideoFeed<ByteSource> {
    let reader = RawAnnexBReader::new(
        pool,
        codec,
        SOURCE,
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        RawAnnexBLimits::default(),
    )
    .unwrap();
    let bytes = ByteSource::new(SOURCE, Bytes::copy_from_slice(pool, bytes).unwrap(), 37).unwrap();
    RawVideoSource::new(reader, bytes)
        .into_feed(4096, clock)
        .unwrap()
}

#[test]
fn raw_feed_delivers_annex_b_and_configuration_with_the_unstamped_queue_value() {
    use triergon_playback::raw_video::RawFeedClock;
    for (codec, bytes) in RAW_FIXTURES {
        let pool = Budget::new(1 << 20).unwrap();
        let mut feed = open_raw_feed(&pool, codec, bytes, RawFeedClock::Unspecified);
        let mut count = 0;
        let mut offset = 0;
        for _ in 0..100_000 {
            match feed.poll(&mut WorkBudget::new(1), &OFF).unwrap() {
                Step::Ready(packet) => {
                    assert_eq!(packet.unit.range.start, offset);
                    offset = packet.unit.range.end;
                    let start = packet.unit.range.start as usize;
                    assert!(
                        packet
                            .unit
                            .segments()
                            .flatten()
                            .copied()
                            .eq(bytes[start..offset as usize].iter().copied())
                    );
                    assert_eq!(packet.configuration_revision, Some(1));
                    assert_eq!(packet.configuration_changed, count == 0);
                    let config = packet.configuration.as_ref().unwrap();
                    assert_eq!(config.codec, codec);
                    let expected_hevc = (codec == Codec::Hevc).then_some(
                        triergon_playback::raw_video::HevcVuiTiming {
                            num_units_in_tick: 1,
                            time_scale: 12,
                            poc_proportional_to_timing_flag: false,
                            num_ticks_poc_diff_one_minus1: None,
                        },
                    );
                    assert_eq!(config.hevc_timing().unwrap(), expected_hevc);
                    assert_eq!(packet.submission_time, None);
                    assert_eq!(packet.queue_timestamp_ns(), 0);
                    count += 1;
                }
                Step::End(at) => {
                    assert_eq!(at.offset, bytes.len() as u64);
                    break;
                }
                Step::Yield | Step::NeedInput(_) => {}
            }
        }
        assert_eq!(count, 12);
        assert_eq!(offset, bytes.len() as u64);
        drop(feed);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn raw_feed_fixed_submission_clock_and_reconfiguration_are_independent() {
    use triergon_core::Time;
    use triergon_playback::raw_video::RawFeedClock;

    let first = RAW_FIXTURES[0].1;
    let second = include_bytes!("fixtures/raw-video/avc-80x48.h264");
    let trailing_aud = [0, 0, 1, 9, 0xf0];
    let input = [first, second, trailing_aud.as_slice()].concat();
    let pool = Budget::new(1 << 20).unwrap();
    let mut feed = open_raw_feed(
        &pool,
        Codec::Avc,
        &input,
        RawFeedClock::FixedSubmission {
            start: Time::new(0, 1).unwrap(),
            picture_duration: Time::new(1001, 24000).unwrap(),
        },
    );
    let mut first_packet = None;
    let mut seen = 0;
    let mut seen_prefix = false;
    for _ in 0..100_000 {
        match feed.poll(&mut WorkBudget::new(29), &OFF).unwrap() {
            Step::Ready(packet) if packet.unit.has_vcl => {
                assert_eq!(
                    packet.submission_time,
                    Some(Time::new(seen * 1001, 24000).unwrap())
                );
                assert_eq!(
                    i128::from(packet.queue_timestamp_ns()),
                    (seen * 1001 * 1_000_000_000 + 12000) / 24000
                );
                let revision = if seen < 12 { 1 } else { 2 };
                assert_eq!(packet.configuration_revision, Some(revision));
                assert_eq!(packet.configuration_changed, seen == 0 || seen == 12);
                let config = packet.configuration.as_ref().unwrap();
                assert_eq!(
                    (config.width, config.height),
                    if seen < 12 { (64, 64) } else { (80, 48) }
                );
                if seen == 0 {
                    first_packet = Some(packet);
                }
                seen += 1;
            }
            Step::Ready(packet) => {
                assert!(!seen_prefix);
                seen_prefix = true;
                assert_eq!(packet.submission_time, None);
                assert!(!packet.configuration_changed);
                assert_eq!(packet.configuration_revision, Some(2));
                assert!(packet.unit.segments().flatten().copied().eq(trailing_aud));
            }
            Step::End(at) => {
                assert_eq!(at.offset, input.len() as u64);
                break;
            }
            Step::Yield | Step::NeedInput(_) => {}
        }
    }
    assert_eq!(seen, 14);
    assert!(seen_prefix);
    let first_packet = first_packet.unwrap();
    assert_eq!(first_packet.configuration.unwrap().width, 64);
    assert_eq!(first_packet.configuration_revision, Some(1));
    drop(feed);
    drop(first_packet.unit);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
