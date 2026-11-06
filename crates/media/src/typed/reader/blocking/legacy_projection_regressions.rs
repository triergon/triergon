//! Source/API/resource controls for the legacy projection.
//! The control is the exact legacy next_packet consumer, not a second parser.
use super::*;
use crate::matroska::typed::usage::{Owner, Storage};
use std::{
    cell::RefCell,
    io::{self, Cursor, SeekFrom},
    rc::Rc,
};

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|x| *x == 0)
        .collect();
    let width = (1..=8)
        .find(|width| (data.len() as u64) < (1_u64 << (7 * width)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend_from_slice(data);
    out
}
fn unsigned(id: u64, n: u64) -> Vec<u8> {
    element(id, &n.to_be_bytes())
}
fn simple(track: u8, ticks: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    element(
        0xa3,
        &[
            vec![0x80 | track],
            ticks.to_be_bytes().to_vec(),
            vec![flags],
            data.to_vec(),
        ]
        .concat(),
    )
}
fn group(extra: &[u8]) -> Vec<u8> {
    element(
        0xa0,
        &[element(0xa1, &[0x81, 0, 1, 0, b'G']), extra.to_vec()].concat(),
    )
}
fn fixture(blocks: &[Vec<u8>]) -> Vec<u8> {
    let track = |number, kind, codec: &[u8]| {
        element(
            0xae,
            &[
                unsigned(0xd7, number),
                unsigned(0x73c5, number),
                unsigned(0x83, kind),
                element(0x86, codec),
                element(0x63a2, b"initial"),
                unsigned(0x55ee, 1),
            ]
            .concat(),
        )
    };
    let tracks = element(
        0x1654ae6b,
        &[
            track(1, 1, b"V_AUTHORED/OPAQUE"),
            track(2, 17, b"S_TEXT/UTF8"),
        ]
        .concat(),
    );
    let cluster = element(0x1f43b675, &[unsigned(0xe7, 0), blocks.concat()].concat());
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &[tracks, cluster].concat()),
    ]
    .concat()
}
fn plain() -> Vec<u8> {
    fixture(&[
        simple(1, 0, 0x80, b"A"),
        simple(2, 0, 0x80, b"subtitle"),
        simple(1, 2, 0x80, b"C"),
    ])
}
fn additions() -> Vec<u8> {
    element(
        0x75a1,
        &element(0xa6, &[unsigned(0xee, 1), element(0xa5, b"extra")].concat()),
    )
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct PacketView {
    track: usize,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<u64>,
    key: bool,
    padding: i64,
    timing: crate::timing::BlockTiming,
    data: Vec<u8>,
    capacity: usize,
}
fn view(p: crate::matroska::Packet) -> PacketView {
    PacketView {
        track: p.track,
        pts: p.pts_ns,
        dts: p.dts_ns,
        duration: p.duration_ns,
        key: p.keyframe,
        padding: p.discard_padding_ns,
        timing: p.timing,
        data: p.data.as_ref().to_vec(),
        capacity: p.data.retained_capacity(),
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum IoCall {
    Read(u64, usize),
    Seek(SeekFrom),
}
struct Recorded {
    source: Cursor<Vec<u8>>,
    calls: Rc<RefCell<Vec<IoCall>>>,
    short: usize,
    fail_at: Option<u64>,
}
impl Read for Recorded {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.calls
            .borrow_mut()
            .push(IoCall::Read(self.source.position(), out.len()));
        if self.fail_at.is_some_and(|at| self.source.position() >= at) {
            return Err(io::Error::from(io::ErrorKind::ConnectionReset));
        }
        let n = out.len().min(self.short);
        self.source.read(&mut out[..n])
    }
}
impl Seek for Recorded {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.calls.borrow_mut().push(IoCall::Seek(position));
        self.source.seek(position)
    }
}
fn open(
    raw: Vec<u8>,
    short: usize,
    fail_at: Option<u64>,
) -> (BlockingReader<Recorded>, Rc<RefCell<Vec<IoCall>>>) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let source = Recorded {
        source: Cursor::new(raw),
        calls: calls.clone(),
        short,
        fail_at,
    };
    (
        BlockingReader::open_seekable(source, Limits::default()).unwrap(),
        calls,
    )
}
fn original_next(
    reader: &mut BlockingReader<Recorded>,
    cancel: &AtomicBool,
) -> Result<Option<crate::matroska::Packet>> {
    let Some(packet) = reader.next_typed_packet(cancel)? else {
        return Ok(None);
    };
    if packet
        .group
        .as_ref()
        .is_some_and(|group| group.additions().len() != 0 || group.codec_state().is_some())
    {
        reader.reader.failed = true;
        return Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet",
        ));
    }
    Ok(Some(packet.packet))
}
fn next_view(
    reader: &mut BlockingReader<Recorded>,
    original: bool,
    cancel: &AtomicBool,
) -> Result<Option<PacketView>> {
    if original {
        original_next(reader, cancel).map(|p| p.map(view))
    } else {
        reader.next_packet(cancel).map(|p| p.map(view))
    }
}
fn compare_all(raw: Vec<u8>, short: usize, fail_at: Option<u64>) {
    let (mut old, old_io) = open(raw.clone(), short, fail_at);
    let (mut new, new_io) = open(raw, short, fail_at);
    let cancel = AtomicBool::new(false);
    for _ in 0..1024 {
        let a = next_view(&mut old, true, &cancel);
        let b = next_view(&mut new, false, &cancel);
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b);
                if a.is_none() {
                    break;
                }
            }
            (Err(a), Err(b)) => {
                assert_eq!(format!("{a:?}"), format!("{b:?}"));
                break;
            }
            other => panic!("projection differs {other:?}"),
        }
    }
    assert_eq!(*old_io.borrow(), *new_io.borrow());
    assert_eq!(old.reader.state_coverage_end, new.reader.state_coverage_end);
    assert_eq!(old.reader.failed, new.reader.failed);
    assert_eq!(old.reader.position(), new.reader.position());
}
#[test]
fn legacy_projection_plain_laced_and_empty_group_keep_packets_io_and_eof() {
    let raw = fixture(&[
        simple(1, 0, 0x80, b"A"),
        simple(1, 1, 0x84, &[1, b'B', b'C']),
        group(&[]),
        simple(2, 3, 0x80, b"text"),
    ]);
    for short in [1, 3, 8192] {
        compare_all(raw.clone(), short, None);
    }
}
#[test]
fn legacy_projection_full_state_and_additions_keep_late_refusal_and_coverage() {
    for extra in [element(0xa4, b"new-state"), additions()] {
        for short in [1, 8192] {
            compare_all(
                fixture(&[
                    simple(1, 0, 0x80, b"A"),
                    group(&extra),
                    simple(1, 2, 0x80, b"C"),
                ]),
                short,
                None,
            );
        }
    }
}
#[test]
fn legacy_projection_refusal_keeps_following_seek_io_receipt_and_source_state() {
    let raw = fixture(&[
        simple(1, 0, 0x80, b"A"),
        group(&element(0xa4, b"new-state")),
        simple(1, 2, 0x80, b"C"),
    ]);
    let (mut old, old_io) = open(raw.clone(), 1, None);
    let (mut new, new_io) = open(raw, 1, None);
    let cancel = AtomicBool::new(false);
    assert_eq!(
        next_view(&mut old, true, &cancel).unwrap(),
        next_view(&mut new, false, &cancel).unwrap()
    );
    assert!(matches!(
        original_next(&mut old, &cancel),
        Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet"
        ))
    ));
    assert!(matches!(
        new.next_packet(&cancel),
        Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet"
        ))
    ));
    assert_eq!(old.reader.state_coverage_end, new.reader.state_coverage_end);
    assert!(new.reader.failed);
    let a = old.seek_previous(0, 0, &cancel).unwrap();
    let b = new.seek_previous(0, 0, &cancel).unwrap();
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(*old_io.borrow(), *new_io.borrow());
    assert_eq!(
        old.reader.configuration.as_ref().unwrap().revision(),
        new.reader.configuration.as_ref().unwrap().revision()
    );
}
#[test]
fn legacy_projection_pending_seek_refusal_is_nonsticky_and_same_seek_can_finish() {
    let (mut reader, _) = open(plain(), 1, None);
    let request = SeekRequest {
        track: 0,
        requested_ns: 0,
        playback_preroll: false,
    };
    reader.reader.seek_previous(0, 0).unwrap();
    let token = reader.reader.request().token;
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(false)),
        Err(Error::Invalid("pending seek must be completed"))
    ));
    assert!(!reader.reader.failed);
    assert_eq!(reader.reader.pending_seek(), Some(request));
    assert_eq!(reader.reader.request().token, token);
    let receipt = reader.seek_previous(0, 0, &AtomicBool::new(false)).unwrap();
    assert_eq!(receipt.actual_ns, 0);
    assert_eq!(
        reader
            .next_packet(&AtomicBool::new(false))
            .unwrap()
            .unwrap()
            .data
            .as_ref(),
        b"A"
    );
}
#[test]
fn legacy_projection_mixed_methods_preserve_retained_all_track_snapshot_after_reset() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader =
        BlockingReader::open_seekable_in(Cursor::new(plain()), Limits::default(), &pool).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        reader.next_packet(&cancel).unwrap().unwrap().data.as_ref(),
        b"A"
    );
    let typed = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(typed.packet.data.as_ref(), b"subtitle");
    assert_eq!(typed.configuration.tracks().len(), 2);
    let generation = typed.configuration.generation();
    let revision = typed.configuration.revision();
    assert_eq!(
        typed.configuration.initialization(0).unwrap().data,
        b"initial"
    );
    assert_eq!(
        reader.next_packet(&cancel).unwrap().unwrap().data.as_ref(),
        b"C"
    );
    assert!(reader.next_packet(&cancel).unwrap().is_none());
    reader.reader.reset().unwrap();
    assert_ne!(reader.reader.generation(), generation);
    drop(reader);
    assert_eq!(typed.configuration.generation(), generation);
    assert_eq!(typed.configuration.revision(), revision);
    assert_eq!(
        typed.configuration.initialization(1).unwrap().data,
        b"initial"
    );
    assert!(pool.used() > baseline);
    drop(typed);
    assert_eq!(pool.used(), baseline);
}
fn snapshot_slot(job: &SimplePacketJob) -> bool {
    (0..job.storage_slots().unwrap()).any(|at| {
        matches!(
            job.storage_owner(at).unwrap(),
            Some(Owner::Configuration(_))
        )
    })
}
fn quantum<D: PacketProjection>(
    reader: &mut BlockingReader<Recorded>,
    units: u64,
) -> Result<ParseStep<D::Event>> {
    if reader.input.has_pending_read() {
        reader.input.discard_stale(&reader.reader.request());
    }
    reader
        .reader
        .set_adapter_storage(reader.input.retained_buffer_bytes());
    let step = reader
        .reader
        .step_projected::<D>(&mut WorkBudget::new(units), &AtomicBool::new(false))?;
    if let ParseStep::NeedInput(request) = step {
        let header = reader.reader.needs_header_input();
        let refill = reader.reader.header_refill_for_request(&request)?;
        let result = if let Some(plan) = refill {
            reader.input.feed_typed_header_refill(
                reader.reader.envelope.range_input(),
                &plan,
                &AtomicBool::new(false),
                reader.reposition,
            )
        } else {
            reader.input.feed_typed_request(
                reader.reader.envelope.range_input(),
                &request,
                header,
                &AtomicBool::new(false),
                reader.reposition,
            )
        }?;
        reader
            .reader
            .set_adapter_storage(reader.input.retained_buffer_bytes());
        if let FeedStep::Backpressure { requested, .. } = result {
            reader.reader.allocation_blocked(requested);
        }
        Ok(ParseStep::Yield)
    } else {
        Ok(step)
    }
}
fn paused_plain<D: PacketProjection>() -> BlockingReader<Recorded> {
    let (mut reader, _) = open(plain(), 1, None);
    for _ in 0..100000 {
        if matches!(&reader.reader.state, State::Simple(_)) {
            return reader;
        }
        let _ = quantum::<D>(&mut reader, 1).unwrap();
    }
    panic!("plain state not reached")
}
#[test]
fn legacy_projection_omits_only_unused_plain_snapshot_and_preserves_typed_owner_graph() {
    let legacy = paused_plain::<LegacyProjection>();
    let typed = paused_plain::<TypedProjection>();
    let State::Simple(a) = &legacy.reader.state else {
        panic!("missing legacy simple")
    };
    let State::Simple(b) = &typed.reader.state else {
        panic!("missing typed simple")
    };
    assert!(!snapshot_slot(a));
    assert!(snapshot_slot(b));
    assert_eq!(legacy.reader.tracks().len(), typed.reader.tracks().len());
    assert!(legacy.reader.configuration.is_some());
    assert_eq!(size_of::<SimplePacketJob>(), size_of_val(a));
}
#[test]
fn legacy_projection_switch_during_real_pressure_does_not_change_walk_graph_before_ready() {
    let mut reader = paused_plain::<LegacyProjection>();
    let pool = reader.budget().clone();
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(pool.reserve(1), Err(Error::Backpressure { .. })));
    reader.reader.allocation_blocked(1);
    for units in [0, 1, 1, 1, 7, 8192] {
        let step = quantum::<TypedProjection>(&mut reader, units).unwrap();
        assert!(matches!(
            step,
            ParseStep::Yield | ParseStep::Backpressure { .. }
        ));
        if let State::Simple(job) = &reader.reader.state {
            assert!(!snapshot_slot(job));
        } else {
            panic!("pressure replaced plain job");
        }
        if reader.reader.pressure.is_none() {
            break;
        }
    }
    drop(blocker);
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"A");
    assert_eq!(packet.configuration.tracks().len(), 2);
}
#[test]
fn legacy_projection_borrowed_lace_promotion_restores_owned_full_snapshot() {
    let raw = fixture(&[
        simple(1, 0, 0x84, &[1, b'A', b'B']),
        simple(1, 2, 0x80, b"C"),
    ]);
    let (mut reader, _) = open(raw, 1, None);
    for _ in 0..100000 {
        if let State::Packet(job) = &reader.reader.state {
            assert!(job.storage_slots().unwrap() > 0);
            break;
        }
        let _ = quantum::<LegacyProjection>(&mut reader, 1).unwrap();
    }
    assert!(matches!(reader.reader.state, State::Packet(_)));
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"A");
    assert_eq!(packet.configuration.tracks().len(), 2);
}
#[test]
fn legacy_projection_zero_one_work_and_cancel_keep_pending_packet_unpublished() {
    let mut reader = paused_plain::<LegacyProjection>();
    let position = reader.reader.position();
    assert!(matches!(
        quantum::<LegacyProjection>(&mut reader, 0).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(reader.reader.position(), position);
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        reader.next_typed_packet(&AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}
#[test]
fn legacy_projection_late_source_failure_and_truncation_match_original_consumer() {
    let raw = plain();
    compare_all(raw.clone(), 1, Some((raw.len() - 1) as u64));
    for cut in [raw.len() - 1, raw.len() - 3] {
        compare_all(raw[..cut].to_vec(), 1, None);
    }
}

// Independent serialized lengths determine CueClusterPosition; the fixed-width
// UInt encoding makes the Cue size invariant while its actual position is set.
fn indexed_second_cluster_additions() -> Vec<u8> {
    let track = |number, kind, codec: &[u8]| {
        element(
            0xae,
            &[
                unsigned(0xd7, number),
                unsigned(0x73c5, number),
                unsigned(0x83, kind),
                element(0x86, codec),
                element(0x63a2, b"initial"),
                unsigned(0x55ee, 1),
            ]
            .concat(),
        )
    };
    let tracks = element(
        0x1654ae6b,
        &[
            track(1, 1, b"V_AUTHORED/OPAQUE"),
            track(2, 17, b"S_TEXT/UTF8"),
        ]
        .concat(),
    );
    let first = element(
        0x1f43b675,
        &[unsigned(0xe7, 0), simple(1, 0, 0x80, b"A")].concat(),
    );
    let second = element(
        0x1f43b675,
        &[unsigned(0xe7, 10), group(&additions())].concat(),
    );
    let cue = |position| {
        element(
            0x1c53bb6b,
            &element(
                0xbb,
                &[
                    unsigned(0xb3, 11),
                    element(
                        0xb7,
                        &[
                            unsigned(0xf7, 1),
                            unsigned(0xf1, position),
                            unsigned(0xea, 0),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        )
    };
    let cues = cue((tracks.len() + cue(0).len() + first.len()) as u64);
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, &[tracks, cues, first, second].concat()),
    ]
    .concat()
}
#[test]
fn legacy_projection_additions_refusal_preserves_noninitial_cue_zero_state_epoch_io() {
    let raw = indexed_second_cluster_additions();
    let (mut old, old_io) = open(raw.clone(), 1, None);
    let (mut new, new_io) = open(raw, 1, None);
    let cancel = AtomicBool::new(false);
    assert_eq!(
        next_view(&mut old, true, &cancel).unwrap(),
        next_view(&mut new, false, &cancel).unwrap()
    );
    assert!(matches!(
        original_next(&mut old, &cancel),
        Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet"
        ))
    ));
    assert!(matches!(
        new.next_packet(&cancel),
        Err(Error::Unsupported(
            "Block additions/CodecState require next_typed_packet"
        ))
    ));
    assert!(!old.reader.state_seen);
    assert!(!new.reader.state_seen);
    assert_eq!(old.reader.state_coverage_end, new.reader.state_coverage_end);
    let a = old.seek_previous(0, 11_000_000, &cancel).unwrap();
    let b = new.seek_previous(0, 11_000_000, &cancel).unwrap();
    assert!(a.used_index && b.used_index);
    assert_eq!(a.actual_ns, 11_000_000);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(*old_io.borrow(), *new_io.borrow());
}
