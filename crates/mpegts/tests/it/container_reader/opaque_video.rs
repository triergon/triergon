use super::{Drive, ID, OFF, PID, Transport};
use std::ops::Range;
use triergon_core::container::Event;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, Step, WorkBudget,
};
use triergon_mpegts::reader::SourceEvent;
use triergon_mpegts::reader::UnframedVideo;
use triergon_mpegts::stream::{FaultKind, TraceEvent};
use triergon_mpegts::transport::ContinuityOutcome;
use triergon_mpegts::{Reader, ReaderEvent, ReaderLimits};

const LEADING_BYTES: Error = Error::Invalid("Annex B leading bytes");

fn report() -> ReaderLimits {
    ReaderLimits {
        unframed_video_policy: UnframedVideo::Report,
        ..ReaderLimits::default()
    }
}

#[test]
fn opaque_avc_and_hevc_report_one_original_run_at_eof() {
    for kind in [0x1b, 0x24] {
        let mut transport = Transport::new();
        transport.tables(&[(kind, PID, &[])], 0);
        transport.pes(PID, 0xe0, b"opaque\0", Some(90_000), None);
        transport.pes(PID, 0xe0, b"\0\x02tail", Some(93_000), None);

        let mut strict = Drive::new(&transport.bytes, ReaderLimits::default(), 23, 1);
        loop {
            match strict.next() {
                Err(error) => {
                    assert_eq!(error, LEADING_BYTES);
                    break;
                }
                Ok(Some(Event::Other(
                    ReaderEvent::Source(_)
                    | ReaderEvent::Transport(_)
                    | ReaderEvent::Continuity { .. },
                ))) => (),
                other => panic!("unexpected strict output: {other:?}"),
            }
        }

        let mut reader = Drive::new(&transport.bytes, report(), 23, 1);
        let mut video = None;
        let mut source_pes = 0;
        let mut finished = false;
        while let Some(event) = reader.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Source(source)) => match source.event() {
                    TraceEvent::Pes(_) => source_pes += 1,
                    TraceEvent::Finished => finished = true,
                    _ => (),
                },
                Event::Other(ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. }) => (),
                Event::Other(ReaderEvent::IncompleteVideo(observation)) => {
                    assert!(!finished);
                    assert_eq!(source_pes, 2);
                    assert!(video.replace(observation).is_none());
                }
                other => panic!("unexpected opaque output: {other:?}"),
            }
        }
        assert!(finished);
        reader.reader.reset().unwrap();
        drop(reader);
        let video = video.unwrap();
        assert_eq!(video.bytes().as_ref(), b"opaque\0\0\x02tail");
        assert_eq!(video.elementary_range(), 0..13);
        assert_eq!(video.track_info().stream().stream_type, kind);
        assert_eq!(video.sources().len(), 2);
        assert_eq!(video.sources()[0].elementary_range, 0..7);
        assert_eq!(video.sources()[1].elementary_range, 7..13);
        assert_eq!(video.clock().unwrap().pts.ticks90, Some(90_000));
        assert_eq!(video.clock().unwrap().dts.ticks90, None);
        for (origin, pts) in video.sources().iter().zip([90_000, 93_000]) {
            let TraceEvent::Pes(pes) = origin.pes.event() else {
                panic!("missing original PES");
            };
            assert_eq!(pes.header.pts_raw33, Some(pts));
            assert_eq!(pes.epochs, video.epochs());
            assert_eq!(origin.payload_range, 0..pes.payload_len);
        }
    }
}

#[test]
fn a_later_start_code_split_across_pes_refuses_the_opaque_run() {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    transport.pes(PID, 0xe0, b"opaque\0", None, None);
    transport.pes(PID, 0xe0, b"\0\x01\x65\x88", None, None);
    let mut reader = Drive::new(&transport.bytes, report(), 7, 1);
    loop {
        match reader.next() {
            Err(error) => {
                assert_eq!(error, LEADING_BYTES);
                break;
            }
            Ok(Some(Event::Other(
                ReaderEvent::Source(_) | ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            ))) => (),
            other => panic!("unexpected opaque output: {other:?}"),
        }
    }
}

#[test]
fn opaque_run_extent_has_a_permanent_limit() {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    transport.pes(PID, 0xe0, b"abcd", None, None);
    transport.pes(PID, 0xe0, b"e", None, None);
    let mut limits = report();
    limits.video.maximum_access_unit_bytes = 4;
    let mut reader = Drive::new(&transport.bytes, limits, 23, 1);
    loop {
        match reader.next() {
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Limit {
                        requested: 5,
                        maximum: 4
                    }
                );
                break;
            }
            Ok(Some(Event::Other(
                ReaderEvent::Source(_) | ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            ))) => (),
            other => panic!("unexpected opaque output: {other:?}"),
        }
    }
}

/// What a reader reports about its video track, in order.
#[derive(Debug, PartialEq)]
enum Seen {
    /// A source PES with its payload length.
    Pes(usize),
    /// An unframed run: its bytes, the PTS of its first PES and whether a source event
    /// ended it.
    Run(Vec<u8>, Option<i128>, Ended),
    /// Unpublished bytes a boundary cut.
    Interrupted(Range<u64>),
    /// A complete access unit.
    Unit(Range<u64>),
    Fault(FaultKind),
    Timeline,
    Map,
}

/// What ended an unframed run.
#[derive(Debug, PartialEq)]
enum Ended {
    Fault(FaultKind),
    /// A signalled discontinuity of the continuity counter.
    Signalled,
    Map,
    Source,
}

/// `route_epoch` is the configuration epoch the video route keeps under a later program map.
fn seen(raw: &[u8], chunk: usize, work: u64, route_epoch: Option<u64>) -> Vec<Seen> {
    let mut reader = Drive::new(raw, report(), chunk, work);
    reader.catalogue_epoch_override = route_epoch;
    let mut seen = Vec::new();
    while let Some(event) = reader.next().unwrap() {
        seen.push(match event {
            Event::Other(ReaderEvent::Source(source)) => match source.event() {
                TraceEvent::Pes(pes) => Seen::Pes(pes.payload_len),
                TraceEvent::Diagnostic(fault) => Seen::Fault(fault.kind),
                TraceEvent::Timeline { .. } => Seen::Timeline,
                TraceEvent::ProgramMap { active: true, .. } => Seen::Map,
                _ => continue,
            },
            Event::Other(ReaderEvent::IncompleteVideo(video)) => Seen::Run(
                video.bytes().as_ref().to_vec(),
                video.clock().and_then(|clock| clock.pts.ticks90),
                match video.ended_by().map(SourceEvent::event) {
                    None => Ended::Source,
                    Some(TraceEvent::Diagnostic(fault)) => Ended::Fault(fault.kind),
                    Some(TraceEvent::Continuity {
                        outcome: ContinuityOutcome::SignalledDiscontinuity,
                        ..
                    }) => Ended::Signalled,
                    Some(TraceEvent::ProgramMap { .. }) => Ended::Map,
                    Some(other) => panic!("unexpected end of a run: {other:?}"),
                },
            ),
            Event::Other(ReaderEvent::Interrupted {
                elementary_range, ..
            }) => Seen::Interrupted(elementary_range),
            Event::Packet(packet) => Seen::Unit(packet.elementary_range()),
            Event::Other(ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. }) => continue,
            other => panic!("unexpected output: {other:?}"),
        });
    }
    seen
}

/// The events of `raw`, which one-byte input under one-unit grants must report alike.
fn seen_under_any_grant(raw: &[u8]) -> Vec<Seen> {
    seen_with_route_epoch(raw, None)
}

fn seen_with_route_epoch(raw: &[u8], route_epoch: Option<u64>) -> Vec<Seen> {
    let whole = seen(raw, 4096, 8192, route_epoch);
    assert_eq!(seen(raw, 1, 1, route_epoch), whole);
    whole
}

/// The payload of the first PES in the fault fixtures of `tests/data`.
const FIXTURE_PES: &[u8] = b"lane-ts-original-payload\0G\xff";

fn run(bytes: &[u8], pts: i128, ended: Ended) -> Seen {
    Seen::Run(bytes.to_vec(), Some(pts), ended)
}

#[test]
fn an_opaque_run_is_reported_before_the_fault_that_ends_it() {
    use Seen::{Fault, Pes};
    for (raw, fault, later) in [
        // A third copy of a carrier.
        (
            &include_bytes!("../../data/cc-third-copy.ts")[..],
            FaultKind::Continuity,
            vec![],
        ),
        // Changed bytes under a repeated counter, and a counter gap at a unit start: the
        // PES the faulted carrier starts opens the next run.
        (
            include_bytes!("../../data/cc-same-counter-changed-payload.ts"),
            FaultKind::Continuity,
            vec![Pes(9), run(b"different", 90_000, Ended::Source)],
        ),
        (
            include_bytes!("../../data/cc-loss-clean-pusi.ts"),
            FaultKind::Continuity,
            vec![Pes(1), run(b"b", 90_000, Ended::Source)],
        ),
        (
            include_bytes!("../../data/continuity-v1/opcr-change-is-not-duplicate.ts"),
            FaultKind::Continuity,
            vec![Pes(27), run(FIXTURE_PES, 90_000, Ended::Source)],
        ),
        // A position without a sync byte and a final partial carrier.
        (
            include_bytes!("../../data/transport-sync-loss.ts"),
            FaultKind::Transport,
            vec![],
        ),
        (
            include_bytes!("../../data/transport-partial-final-packet.ts"),
            FaultKind::Transport,
            vec![],
        ),
        // Payload past the declared length of a finite PES.
        (
            include_bytes!("../../data/pes-finite-extra-payload.ts"),
            FaultKind::Pes,
            vec![],
        ),
    ] {
        let mut expected = vec![
            Seen::Map,
            Pes(27),
            run(FIXTURE_PES, 90_000, Ended::Fault(fault)),
            Fault(fault),
        ];
        expected.extend(later);
        assert_eq!(seen_under_any_grant(raw), expected);
    }
}

#[test]
fn a_new_clock_epoch_separates_two_opaque_runs() {
    assert_eq!(
        seen_under_any_grant(include_bytes!("../../data/pcr-timeline-discontinuity.ts")),
        [
            Seen::Map,
            Seen::Pes(27),
            run(FIXTURE_PES, 90_000, Ended::Signalled),
            Seen::Timeline,
            Seen::Pes(3),
            run(b"new", 10, Ended::Source),
        ]
    );
}

#[test]
fn a_program_map_ends_an_opaque_run_unless_it_keeps_the_route() {
    let audio: (u8, u16, &[u8]) = (0x0f, PID + 1, &[]);
    for (streams, expected) in [
        // The same streams under a new version are a source boundary.
        (
            &[(0x1b, PID, &[][..])][..],
            vec![
                run(b"old", 90_000, Ended::Map),
                Seen::Map,
                Seen::Pes(3),
                run(b"new", 10, Ended::Source),
            ],
        ),
        // A further stream leaves the video route and its run as they are.
        (
            &[(0x1b, PID, &[][..]), audio][..],
            vec![
                Seen::Map,
                Seen::Pes(3),
                run(b"oldnew", 90_000, Ended::Source),
            ],
        ),
    ] {
        let mut transport = Transport::new();
        transport.tables(&[(0x1b, PID, &[])], 0);
        transport.pes(PID, 0xe0, b"old", Some(90_000), None);
        transport.tables(streams, 1);
        transport.pes(PID, 0xe0, b"new", Some(10), None);
        let mut all = vec![Seen::Map, Seen::Pes(3)];
        all.extend(expected);
        // The video route is the first map's in both cases, so it keeps that map's epoch.
        assert_eq!(seen_with_route_epoch(&transport.bytes, Some(2)), all);
    }
}

#[test]
fn a_fault_still_interrupts_a_framed_access_unit() {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    // An access unit delimiter and a slice: no later unit shows that this one is complete.
    transport.pes(
        PID,
        0xe0,
        b"\0\0\x01\x09\x10\0\0\x01\x65\x88",
        Some(90_000),
        None,
    );
    transport.bytes.extend_from_slice(&[0; 188]);
    assert_eq!(
        seen_under_any_grant(&transport.bytes),
        [
            Seen::Map,
            Seen::Pes(10),
            Seen::Interrupted(0..10),
            Seen::Fault(FaultKind::Transport),
        ]
    );
}

/// A reader over `raw` with a pool of `limit` bytes, fed one carrier at a time.
struct Tight<'a> {
    reader: Reader,
    pool: Budget,
    raw: &'a [u8],
}

/// What a tight reader stopped at.
enum Stop {
    /// The reader asks for the bytes at the pause offset.
    Paused(InputRequest),
    Run(Vec<u8>),
    Pressure,
}

impl<'a> Tight<'a> {
    fn new(raw: &'a [u8], limit: usize) -> Self {
        let pool = Budget::new(limit).unwrap();
        let caps = SourceCapabilities {
            length: None,
            random_access: false,
        };
        Self {
            reader: Reader::new(&pool, ID, caps, report(), 188).unwrap(),
            pool,
            raw,
        }
    }

    fn supply(&mut self, request: InputRequest) -> Result<(), Error> {
        let at = usize::try_from(request.offset).unwrap();
        let end = self.raw.len().min(at + request.maximum);
        let bytes = Bytes::copy_from_slice(&self.pool, &self.raw[at..end])?;
        self.reader.provide(&InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes,
        )?)
    }

    /// Reads to the next unframed run or refusal for pressure, or to the request for the
    /// bytes at `pause`, which stays unanswered.
    fn read(&mut self, pause: Option<u64>) -> Result<Stop, Error> {
        loop {
            match self.reader.step(&mut WorkBudget::new(64), &OFF)? {
                Step::NeedInput(request) if pause == Some(request.offset) => {
                    return Ok(Stop::Paused(request));
                }
                Step::NeedInput(request) => self.supply(request)?,
                Step::Ready(Event::Other(ReaderEvent::IncompleteVideo(video))) => {
                    return Ok(Stop::Run(video.bytes().as_ref().to_vec()));
                }
                Step::Backpressure(_) => return Ok(Stop::Pressure),
                Step::Yield | Step::Ready(_) => (),
                Step::End(_) => panic!("the source ended without an unframed run"),
            }
        }
    }
}

#[test]
fn a_run_refused_for_pressure_at_a_fault_is_reported_after_the_caller_releases() {
    let (raw, fault) = run_then_sync_loss();
    let mut tight = Tight::new(&raw, 1 << 20);
    let Stop::Paused(request) = tight.read(Some(fault)).unwrap() else {
        panic!("no request for the faulted carrier");
    };
    tight.supply(request).unwrap();
    // Leave room for the diagnostic, but not for the copy of the run.
    let free = tight.pool.limit() - tight.pool.used();
    let held = tight.pool.reserve(free - RUN_BYTES / 2).unwrap();
    assert!(matches!(tight.read(None).unwrap(), Stop::Pressure));
    drop(held);
    let Stop::Run(bytes) = tight.read(None).unwrap() else {
        panic!("no run after the caller released its reservation");
    };
    assert_eq!(bytes, vec![0xaa; RUN_BYTES]);
}

#[test]
fn a_run_that_cannot_fit_beside_what_the_reader_holds_at_a_fault_is_a_limit() {
    let (raw, fault) = run_then_sync_loss();
    // What the reader holds when the faulted carrier is due; the caller holds nothing.
    let mut probe = Tight::new(&raw, 1 << 20);
    assert!(matches!(probe.read(Some(fault)).unwrap(), Stop::Paused(_)));
    let held = probe.pool.used();
    // Room beside it for the diagnostic, but not for a copy of the run: releasing output
    // cannot help, so the reader reports a limit and never backpressure.
    let mut tight = Tight::new(&raw, held + RUN_BYTES / 2);
    let Stop::Paused(request) = tight.read(Some(fault)).unwrap() else {
        panic!("no request for the faulted carrier");
    };
    tight.supply(request).unwrap();
    assert!(matches!(
        tight.read(None),
        Err(Error::Limit { requested, maximum }) if requested > maximum
    ));
}

const RUN_BYTES: usize = 16 * 4096;

/// Sixteen opaque PES, then a position without a sync byte, and that position.
fn run_then_sync_loss() -> (Vec<u8>, u64) {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    for _ in 0..16 {
        transport.pes(PID, 0xe0, &[0xaa; 4096], None, None);
    }
    let fault = transport.bytes.len() as u64;
    transport.bytes.extend_from_slice(&[0; 188]);
    (transport.bytes, fault)
}
